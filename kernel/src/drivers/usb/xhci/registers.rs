use core::ptr;

/// xHCI Capability Registers (read-only, offsets from BAR base)
#[repr(C)]
pub struct CapabilityRegisters {
    pub caplength: u8,   // 0x00 - Capability Register Length
    _rsvd: u8,           // 0x01
    pub hciversion: u16, // 0x02 - Interface Version Number
    pub hcsparams1: u32, // 0x04 - Structural Parameters 1
    pub hcsparams2: u32, // 0x08 - Structural Parameters 2
    pub hcsparams3: u32, // 0x0C - Structural Parameters 3
    pub hccparams1: u32, // 0x10 - Capability Parameters 1
    pub dboff: u32,      // 0x14 - Doorbell Offset
    pub rtsoff: u32,     // 0x18 - Runtime Register Space Offset
    pub hccparams2: u32, // 0x1C - Capability Parameters 2
}

/// xHCI Operational Registers (offsets from BAR base + CAPLENGTH)
#[repr(C)]
pub struct OperationalRegisters {
    pub usbcmd: u32,    // 0x00
    pub usbsts: u32,    // 0x04
    pub pagesize: u32,  // 0x08
    _rsvd1: [u32; 2],   // 0x0C-0x13
    pub dnctrl: u32,    // 0x14
    pub crcr_lo: u32,   // 0x18 - Command Ring Control (low)
    pub crcr_hi: u32,   // 0x1C - Command Ring Control (high)
    _rsvd2: [u32; 4],   // 0x20-0x2F
    pub dcbaap_lo: u32, // 0x30 - Device Context Base Address Array Pointer (low)
    pub dcbaap_hi: u32, // 0x34 - Device Context Base Address Array Pointer (high)
    pub config: u32,    // 0x38 - Configure
}

/// xHCI Port Register Set (one per port, 16 bytes each)
#[repr(C)]
pub struct PortRegisters {
    pub portsc: u32,    // 0x00 - Port Status and Control
    pub portpmsc: u32,  // 0x04 - Port Power Management Status and Control
    pub portli: u32,    // 0x08 - Port Link Info
    pub porthlpmc: u32, // 0x0C - Port Hardware LPM Control
}

/// xHCI Interrupter Register Set (32 bytes each)
#[repr(C)]
pub struct InterrupterRegisters {
    pub iman: u32,      // 0x00 - Interrupter Management
    pub imod: u32,      // 0x04 - Interrupter Moderation
    pub erstsz: u32,    // 0x08 - Event Ring Segment Table Size
    _rsvd: u32,         // 0x0C
    pub erstba_lo: u32, // 0x10 - Event Ring Segment Table Base Address (low)
    pub erstba_hi: u32, // 0x14 - Event Ring Segment Table Base Address (high)
    pub erdp_lo: u32,   // 0x18 - Event Ring Dequeue Pointer (low)
    pub erdp_hi: u32,   // 0x1C - Event Ring Dequeue Pointer (high)
}

/// Read a volatile u32 from a register pointer.
///
/// # Safety
/// The caller must ensure `ptr` points to a valid, mapped MMIO register.
pub unsafe fn reg_read(ptr: *const u32) -> u32 {
    // SAFETY: the caller guarantees `ptr` is a mapped MMIO register, so a
    // volatile read of it is a well-formed load that the compiler must not
    // fold away or reorder against the other register accesses around it.
    unsafe { ptr::read_volatile(ptr) }
}

/// Write a volatile u32 to a register pointer.
///
/// # Safety
/// The caller must ensure `ptr` points to a valid, mapped MMIO register.
pub unsafe fn reg_write(ptr: *mut u32, val: u32) {
    // SAFETY: the caller guarantees `ptr` is a mapped MMIO register, so a
    // volatile store to it is a well-formed store the compiler must emit
    // exactly once and must not reorder against neighbouring accesses.
    unsafe { ptr::write_volatile(ptr, val) }
}

/// Typed accessor for all xHCI register blocks.
pub struct XhciRegisters {
    base: *mut u8,
    cap_length: u8,
    db_offset: u32,
    rts_offset: u32,
    max_ports: u8,
}

// SAFETY: `base` is a unique MMIO mapping this value owns, reached only
// through the volatile accessors above. `Send` only: nothing here serializes
// two threads against one register block, so it is never shared.
unsafe impl Send for XhciRegisters {}

impl XhciRegisters {
    /// Create from the BAR0 MMIO base pointer.
    ///
    /// Reads CAPLENGTH, DBOFF, and RTSOFF from the capability registers.
    ///
    /// # Safety
    /// `base` must be a valid, mapped pointer to the xHCI BAR0 MMIO region.
    pub unsafe fn new(base: *mut u8) -> Self {
        let cap = base as *const CapabilityRegisters;
        // SAFETY: the caller guarantees `base` points to a mapped xHCI BAR0, and
        // the capability registers sit at offset 0 of it, so `cap` is in bounds
        // for every field read here. All four are read-only hardware values.
        let (cap_length, db_offset, rts_offset, hcsparams1) = unsafe {
            (
                ptr::read_volatile(&(*cap).caplength),
                ptr::read_volatile(&(*cap).dboff),
                ptr::read_volatile(&(*cap).rtsoff),
                ptr::read_volatile(&(*cap).hcsparams1),
            )
        };
        let max_ports = ((hcsparams1 >> 24) & 0xFF) as u8;

        Self {
            base,
            cap_length,
            db_offset,
            rts_offset,
            max_ports,
        }
    }

    pub fn cap(&self) -> *const CapabilityRegisters {
        self.base as *const CapabilityRegisters
    }

    pub fn op(&self) -> *mut OperationalRegisters {
        // SAFETY: `base` is a mapped BAR0 and CAPLENGTH, read from the controller
        // itself, is the offset of the operational registers within it (xHCI 1.2
        // §5.3.1), so the result stays inside the same mapping.
        unsafe { self.base.add(self.cap_length as usize) as *mut OperationalRegisters }
    }

    /// Return a pointer to the port register set for the given zero-based port index.
    ///
    /// Port registers start at op_base + 0x400, each set is 16 bytes.
    pub fn port(&self, index: u8) -> *mut PortRegisters {
        assert!(index < self.max_ports, "port index out of range");
        // SAFETY: the port register sets are a `max_ports`-long array at
        // op_base + 0x400 (xHCI 1.2 §5.4.8), and the assert above bounds `index`
        // to that array, so the offset stays inside the BAR0 mapping.
        unsafe {
            self.base
                .add(self.cap_length as usize + 0x400 + (index as usize) * 16)
                as *mut PortRegisters
        }
    }

    pub fn doorbell(&self, index: u8) -> *mut u32 {
        // SAFETY: `db_offset` is DBOFF as the controller reported it, and the
        // doorbell array that starts there is indexed by slot id, of which the
        // driver never allocates more than MaxSlots, so this stays in the BAR.
        unsafe {
            self.base
                .add(self.db_offset as usize + (index as usize) * 4) as *mut u32
        }
    }

    /// Return a pointer to the interrupter register set for the given index.
    ///
    /// Runtime registers are at base + rts_offset; interrupters start at +0x20, each 32 bytes.
    pub fn interrupter(&self, index: u16) -> *mut InterrupterRegisters {
        // SAFETY: `rts_offset` is RTSOFF as the controller reported it, and the
        // interrupter array begins 0x20 into the runtime registers (xHCI 1.2
        // §5.5). This driver only ever uses interrupter 0, well inside the BAR.
        unsafe {
            self.base
                .add(self.rts_offset as usize + 0x20 + (index as usize) * 32)
                as *mut InterrupterRegisters
        }
    }

    pub fn max_ports(&self) -> u8 {
        self.max_ports
    }
}
