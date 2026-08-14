use crate::drivers::dma::{DmaBuffer, dma};

/// Transfer Request Block - the fundamental xHCI data structure (16 bytes).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Trb {
    pub parameter: u64, // meaning depends on TRB type
    pub status: u32,    // status/length
    pub control: u32,   // type (bits [15:10]), cycle bit (bit 0), other flags
}

impl Trb {
    pub fn trb_type(&self) -> u8 {
        ((self.control >> 10) & 0x3F) as u8
    }

    pub fn cycle_bit(&self) -> bool {
        self.control & 1 != 0
    }

    /// Enable Slot Command (type 9).
    /// The assigned slot ID is returned in bits [31:24] of the completion event's control field.
    pub fn enable_slot() -> Self {
        Self {
            parameter: 0,
            status: 0,
            control: (TRB_TYPE_ENABLE_SLOT as u32) << 10,
        }
    }

    /// Address Device Command (type 11).
    ///
    /// `input_ctx_phys` — physical address of the Input Context structure.
    /// `slot_id` — slot ID received from the Enable Slot completion event.
    /// `bsr` — Block Set address Request; when true the SET_ADDRESS USB request is
    ///   not sent to the device (useful for reset-recovery flows).
    pub fn address_device(input_ctx_phys: u64, slot_id: u8, bsr: bool) -> Self {
        Self {
            parameter: input_ctx_phys,
            status: 0,
            control: ((TRB_TYPE_ADDRESS_DEVICE as u32) << 10)
                | ((slot_id as u32) << 24)
                | if bsr { 1 << 9 } else { 0 },
        }
    }

    /// Configure Endpoint Command (type 12).
    ///
    /// `input_ctx_phys` — physical address of the Input Context structure.
    /// `slot_id` — slot ID of the device being configured.
    pub fn configure_endpoint(input_ctx_phys: u64, slot_id: u8) -> Self {
        Self {
            parameter: input_ctx_phys,
            status: 0,
            control: ((TRB_TYPE_CONFIGURE_ENDPOINT as u32) << 10) | ((slot_id as u32) << 24),
        }
    }
}

// TRB type constants (xHCI 1.2 table 6-91)
pub const TRB_TYPE_LINK: u8 = 6;
#[expect(dead_code, reason = "TRB type table entry, transcribed whole")]
pub const TRB_TYPE_NO_OP: u8 = 8;
pub const TRB_TYPE_ENABLE_SLOT: u8 = 9;
pub const TRB_TYPE_ADDRESS_DEVICE: u8 = 11;
pub const TRB_TYPE_CONFIGURE_ENDPOINT: u8 = 12;
pub const TRB_TYPE_TRANSFER: u8 = 32;
pub const TRB_TYPE_COMMAND_COMPLETION: u8 = 33;
pub const TRB_TYPE_PORT_STATUS_CHANGE: u8 = 34;

// TRB control bit positions
pub const TRB_CYCLE: u32 = 1 << 0;
pub const TRB_TOGGLE_CYCLE: u32 = 1 << 1; // For Link TRBs
#[expect(dead_code, reason = "TRB control bit, transcribed whole")]
pub const TRB_CHAIN: u32 = 1 << 4;

// Transfer-stage TRB types
pub const TRB_TYPE_NORMAL: u8 = 1;
pub const TRB_TYPE_SETUP_STAGE: u8 = 2;
pub const TRB_TYPE_DATA_STAGE: u8 = 3;
pub const TRB_TYPE_STATUS_STAGE: u8 = 4;

// Transfer TRB flags
pub const TRB_IDT: u32 = 1 << 6; // Immediate Data (setup packet bytes in parameter field)
pub const TRB_IOC: u32 = 1 << 5; // Interrupt On Completion
pub const TRB_DIR_IN: u32 = 1 << 16; // Direction: 1 = IN (device-to-host)

// Completion codes
pub const COMP_SUCCESS: u8 = 1;
pub const COMP_SHORT_PACKET: u8 = 13;

/// Producer ring — used for both command and transfer rings.
///
/// The last TRB in the ring is always a Link TRB pointing back to the start.
/// Both `CommandRing` and `TransferRing` are type aliases for this type.
pub struct ProducerRing {
    dma: DmaBuffer,
    trbs: *mut Trb,
    size: usize,
    enqueue_idx: usize,
    cycle_bit: bool,
}

// Safety: ProducerRing owns its DMA region and is accessed only from the driver thread.
unsafe impl Send for ProducerRing {}

impl ProducerRing {
    pub fn new(size: usize) -> Self {
        let byte_size = size * core::mem::size_of::<Trb>();
        let dma = dma()
            .allocate_sized(byte_size)
            .expect("xhci: failed to allocate producer ring");
        let trbs = dma.as_ptr() as *mut Trb;
        let phys = dma.phys_addr().as_u64();

        // Zero all TRBs (allocate_sized already zeros, but be explicit)
        unsafe { core::ptr::write_bytes(trbs, 0, size) };

        // Set up Link TRB in the last slot: points back to ring start, with Toggle Cycle.
        // The cycle bit on the Link TRB starts as 0 and will be set when the ring wraps.
        let link_idx = size - 1;
        unsafe {
            let link = &mut *trbs.add(link_idx);
            link.parameter = phys;
            link.status = 0;
            link.control = ((TRB_TYPE_LINK as u32) << 10) | TRB_TOGGLE_CYCLE;
        }

        Self {
            dma,
            trbs,
            size,
            enqueue_idx: 0,
            cycle_bit: true, // Producer starts with cycle=1
        }
    }

    /// Push a TRB onto the ring. Returns the physical address of the placed TRB.
    pub fn push(&mut self, mut trb: Trb) -> u64 {
        if self.cycle_bit {
            trb.control |= TRB_CYCLE;
        } else {
            trb.control &= !TRB_CYCLE;
        }

        let idx = self.enqueue_idx;
        unsafe { core::ptr::write_volatile(self.trbs.add(idx), trb) };

        let trb_phys = self.dma.phys_addr().as_u64() + (idx * core::mem::size_of::<Trb>()) as u64;

        // Advance enqueue pointer, wrapping through the Link TRB
        self.enqueue_idx += 1;
        if self.enqueue_idx >= self.size - 1 {
            // Update the Link TRB's cycle bit via volatile read-modify-write.
            // The Link TRB control field is at byte offset 12 within the TRB (the 4th u32).
            unsafe {
                let link_ptr = self.trbs.add(self.size - 1) as *mut u32;
                let ctrl_ptr = link_ptr.add(3); // control is the 4th u32 (offset 12)
                let mut ctrl = core::ptr::read_volatile(ctrl_ptr);
                if self.cycle_bit {
                    ctrl |= TRB_CYCLE;
                } else {
                    ctrl &= !TRB_CYCLE;
                }
                core::ptr::write_volatile(ctrl_ptr, ctrl);
            }
            self.cycle_bit = !self.cycle_bit;
            self.enqueue_idx = 0;
        }

        trb_phys
    }

    /// Physical address of the ring base.
    pub fn phys_addr(&self) -> u64 {
        self.dma.phys_addr().as_u64()
    }
}

/// Command Ring — issues commands to the xHCI controller (type alias for ProducerRing).
pub type CommandRing = ProducerRing;

/// Transfer Ring — carries data transfers on device endpoints (type alias for ProducerRing).
pub type TransferRing = ProducerRing;

/// Event Ring Segment Table Entry (16 bytes, as specified by xHCI spec).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ErstEntry {
    pub ring_segment_base: u64,
    pub ring_segment_size: u16,
    _rsvd: [u8; 6],
}

/// Event Ring - the controller writes event TRBs here for software to consume.
pub struct EventRing {
    ring_dma: DmaBuffer, // The actual TRB ring
    erst_dma: DmaBuffer, // Event Ring Segment Table (one entry)
    trbs: *mut Trb,
    size: usize,
    dequeue_idx: usize,
    cycle_bit: bool, // Expected cycle bit from hardware
}

// Safety: EventRing owns its DMA regions and is accessed only from the driver thread.
unsafe impl Send for EventRing {}

impl EventRing {
    pub fn new(size: usize) -> Self {
        let ring_bytes = size * core::mem::size_of::<Trb>();
        let ring_dma = dma()
            .allocate_sized(ring_bytes)
            .expect("xhci: failed to allocate event ring");
        let trbs = ring_dma.as_ptr() as *mut Trb;

        // Zero all TRBs so cycle bits start as 0; we poll expecting cycle=1 first
        unsafe { core::ptr::write_bytes(trbs, 0, size) };

        // Allocate ERST with one entry
        let erst_dma = dma()
            .allocate_sized(core::mem::size_of::<ErstEntry>())
            .expect("xhci: failed to allocate ERST");
        let erst_entry = erst_dma.as_ptr() as *mut ErstEntry;
        unsafe {
            (*erst_entry).ring_segment_base = ring_dma.phys_addr().as_u64();
            (*erst_entry).ring_segment_size = size as u16;
            core::ptr::write_bytes(&mut (*erst_entry)._rsvd as *mut u8, 0, 6);
        }

        Self {
            ring_dma,
            erst_dma,
            trbs,
            size,
            dequeue_idx: 0,
            cycle_bit: true, // Hardware starts producing with cycle=1
        }
    }

    /// Check if an event TRB is ready without consuming it.
    pub fn peek(&self) -> bool {
        let trb = unsafe { core::ptr::read_volatile(self.trbs.add(self.dequeue_idx)) };
        trb.cycle_bit() == self.cycle_bit
    }

    /// Poll for the next event TRB. Returns `Some(trb)` if one is ready.
    pub fn poll(&mut self) -> Option<Trb> {
        let trb = unsafe { core::ptr::read_volatile(self.trbs.add(self.dequeue_idx)) };

        if trb.cycle_bit() == self.cycle_bit {
            self.dequeue_idx += 1;
            if self.dequeue_idx >= self.size {
                self.dequeue_idx = 0;
                self.cycle_bit = !self.cycle_bit;
            }
            Some(trb)
        } else {
            None
        }
    }

    /// Physical address of the ERST (write to ERSTBA).
    pub fn erst_phys(&self) -> u64 {
        self.erst_dma.phys_addr().as_u64()
    }

    /// Current dequeue pointer physical address (write to ERDP after consuming events).
    pub fn dequeue_phys(&self) -> u64 {
        self.ring_dma.phys_addr().as_u64() + (self.dequeue_idx * core::mem::size_of::<Trb>()) as u64
    }
}
