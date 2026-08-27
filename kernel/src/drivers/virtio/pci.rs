use x86_64::{
    PhysAddr,
    structures::paging::{PageTableFlags, mapper::MapToError},
};

use crate::{
    drivers::pci::{
        config::{pci_read_u8, pci_read_u16, pci_read_u32, pci_write_u16, read_bar_phys},
        structures::{PciAddress, PciDevice},
    },
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
};

// -------- Virtio PCI capability types --------

pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

// -------- Virtio status bits --------

pub const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
pub const VIRTIO_STATUS_DRIVER: u8 = 2;
pub const VIRTIO_STATUS_DRIVER_OK: u8 = 4;
pub const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
pub const VIRTIO_STATUS_FAILED: u8 = 128;

// -------- Offsets within common configuration structure (MMIO) --------

const COMMON_DFSELECT: usize = 0x00; // u32 - device feature select
const COMMON_DF: usize = 0x04; // u32 - device feature bits
const COMMON_GFSELECT: usize = 0x08; // u32 - guest (driver) feature select
const COMMON_GF: usize = 0x0C; // u32 - guest feature bits
const COMMON_MSIX_CONFIG: usize = 0x10; // u16
const COMMON_NUM_QUEUES: usize = 0x12; // u16
const COMMON_STATUS: usize = 0x14; // u8
const COMMON_QUEUE_SELECT: usize = 0x16; // u16
const COMMON_QUEUE_SIZE: usize = 0x18; // u16
const COMMON_QUEUE_MSIX: usize = 0x1A; // u16
const COMMON_QUEUE_ENABLE: usize = 0x1C; // u16
const COMMON_QUEUE_NOTIFY_OFF: usize = 0x1E; // u16
const COMMON_QUEUE_DESC: usize = 0x20; // u64
const COMMON_QUEUE_AVAIL: usize = 0x28; // u64
const COMMON_QUEUE_USED: usize = 0x30; // u64

// -------- Virtio PCI capability --------

/// A parsed virtio PCI capability structure read from PCI config space.
#[derive(Debug, Clone, Copy)]
pub struct VirtioPciCap {
    pub cfg_type: u8,
    pub bar: u8,
    pub offset: u32,
    pub length: u32,
}

impl VirtioPciCap {
    /// Read a VirtioPciCap from PCI config space at the given capability offset.
    ///
    /// Layout (offsets relative to the capability start):
    ///   +0: cap_vndr (0x09)
    ///   +1: cap_next
    ///   +2: cap_len
    ///   +3: cfg_type
    ///   +4: bar
    ///   +8: offset (u32)
    ///  +12: length (u32)
    fn read_from_config(addr: PciAddress, cap_offset: u8) -> Self {
        let cfg_type = pci_read_u8(addr, cap_offset + 3);
        let bar = pci_read_u8(addr, cap_offset + 4);
        let offset = pci_read_u32(addr, cap_offset + 8);
        let length = pci_read_u32(addr, cap_offset + 12);
        Self {
            cfg_type,
            bar,
            offset,
            length,
        }
    }
}

// -------- BAR mapping helpers --------

/// Map a BAR region into virtual address space using the HHDM offset.
///
/// Already-mapped pages are silently accepted (same as the AHCI controller).
fn map_bar(phys_base: PhysAddr, size: u32) -> *mut u8 {
    let virt = get_virt_addr_from_phys_offset(phys_base);

    {
        let mut mapper = memory_mapper();
        let result = mapper.map_address_range(
            virt,
            phys_base,
            size as usize,
            PageTableFlags::WRITABLE | PageTableFlags::NO_CACHE | PageTableFlags::GLOBAL,
        );
        match result {
            Ok(_) => {}
            Err(MapToError::PageAlreadyMapped(_)) => {}
            Err(e) => {
                panic!("virtio: failed to map BAR at {phys_base:#x}: {e:?}");
            }
        }
    }

    virt.as_mut_ptr()
}

// -------- VirtioTransport --------

/// Virtio PCI modern transport.
///
/// Wraps the MMIO regions discovered through PCI virtio capabilities and
/// provides typed accessors for the common configuration structure, notify
/// region, ISR status, and device-specific configuration.
pub struct VirtioTransport {
    pci_addr: PciAddress,
    common_cfg: *mut u8,
    notify_base: *mut u8,
    notify_off_multiplier: u32,
    #[expect(unused)]
    isr: *mut u8,
    device_cfg: *mut u8,
}

// SAFETY: the four pointers address MMIO regions mapped for the device's
// whole life and reached only through volatile accesses. `Send` only: nothing
// here serializes two threads against one transport, so it is never shared.
unsafe impl Send for VirtioTransport {}

impl VirtioTransport {
    /// Probe and initialise a virtio PCI modern transport from the given device.
    ///
    /// Returns `None` if the required capabilities (common cfg, notify) are
    /// not found in the PCI capability list.
    pub fn new(pci_device: &PciDevice) -> Option<Self> {
        let addr = pci_device.address;

        let mut common_cfg_ptr: *mut u8 = core::ptr::null_mut();
        let mut notify_base_ptr: *mut u8 = core::ptr::null_mut();
        let mut notify_off_multiplier: u32 = 0;
        let mut isr_ptr: *mut u8 = core::ptr::null_mut();
        let mut device_cfg_ptr: *mut u8 = core::ptr::null_mut();

        // Walk the PCI capability list.
        let mut cap_ptr = pci_read_u8(addr, 0x34); // capabilities pointer
        let mut guard = 0u8;

        while cap_ptr != 0 && guard < 64 {
            let cap_vndr = pci_read_u8(addr, cap_ptr);
            let cap_next = pci_read_u8(addr, cap_ptr + 1);

            if cap_vndr == 0x09 {
                // Virtio PCI capability
                let cap = VirtioPciCap::read_from_config(addr, cap_ptr);

                if cap.bar > 5 {
                    cap_ptr = cap_next;
                    continue;
                }

                // Compute the BAR physical base and map it.
                let bar_phys = read_bar_phys(addr, cap.bar);
                if bar_phys.as_u64() != 0 {
                    let bar_virt = map_bar(bar_phys, cap.offset + cap.length);
                    let mmio_ptr = unsafe { bar_virt.add(cap.offset as usize) };

                    match cap.cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => {
                            common_cfg_ptr = mmio_ptr;
                        }
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            notify_base_ptr = mmio_ptr;
                            // notify_off_multiplier is the u32 immediately after
                            // the standard 16-byte VirtioPciCap fields.
                            notify_off_multiplier = pci_read_u32(addr, cap_ptr + 16);
                        }
                        VIRTIO_PCI_CAP_ISR_CFG => {
                            isr_ptr = mmio_ptr;
                        }
                        VIRTIO_PCI_CAP_DEVICE_CFG => {
                            device_cfg_ptr = mmio_ptr;
                        }
                        _ => {}
                    }
                }
            }

            cap_ptr = cap_next;
            guard += 1;
        }

        if common_cfg_ptr.is_null() || notify_base_ptr.is_null() {
            return None;
        }

        // Enable PCI bus mastering (command register bit 2).
        let cmd = pci_read_u16(addr, 0x04);
        pci_write_u16(addr, 0x04, cmd | (1 << 2));

        Some(Self {
            pci_addr: addr,
            common_cfg: common_cfg_ptr,
            notify_base: notify_base_ptr,
            notify_off_multiplier,
            isr: isr_ptr,
            device_cfg: device_cfg_ptr,
        })
    }

    // ---- MMIO helpers (handle unaligned base pointers safely) ----

    /// Volatile read a u32 from common_cfg at the given byte offset.
    ///
    /// Uses inline asm to avoid Rust's alignment UB check on `write_volatile`
    /// when the pointer is derived from a `*mut u8` base (the VirtIO spec
    /// guarantees DWORD alignment, but Rust's provenance tracking doesn't
    /// carry that info through `u8` pointer arithmetic).
    fn cfg_read_u32(&self, offset: usize) -> u32 {
        let addr = self.common_cfg as usize + offset;
        let val: u32;
        unsafe {
            core::arch::asm!("mov {0:e}, [{1}]", out(reg) val, in(reg) addr, options(nostack, readonly))
        };
        val
    }

    /// Volatile write a u32 to common_cfg at the given byte offset.
    fn cfg_write_u32(&self, offset: usize, val: u32) {
        let addr = self.common_cfg as usize + offset;
        unsafe {
            core::arch::asm!("mov [{1}], {0:e}", in(reg) val, in(reg) addr, options(nostack))
        };
    }

    /// Volatile read a u16 from common_cfg at the given byte offset.
    fn cfg_read_u16(&self, offset: usize) -> u16 {
        let addr = self.common_cfg as usize + offset;
        let val: u16;
        unsafe {
            core::arch::asm!("mov {0:x}, [{1}]", out(reg) val, in(reg) addr, options(nostack, readonly))
        };
        val
    }

    /// Volatile write a u16 to common_cfg at the given byte offset.
    fn cfg_write_u16(&self, offset: usize, val: u16) {
        let addr = self.common_cfg as usize + offset;
        unsafe {
            core::arch::asm!("mov [{1}], {0:x}", in(reg) val, in(reg) addr, options(nostack))
        };
    }

    /// Volatile write a u64 to common_cfg at the given byte offset (two 32-bit writes).
    fn cfg_write_u64(&self, offset: usize, val: u64) {
        self.cfg_write_u32(offset, val as u32);
        self.cfg_write_u32(offset + 4, (val >> 32) as u32);
    }

    // ---- Common configuration accessors (volatile MMIO) ----

    pub fn read_status(&self) -> u8 {
        unsafe { core::ptr::read_volatile(self.common_cfg.add(COMMON_STATUS)) }
    }

    pub fn write_status(&self, val: u8) {
        unsafe { core::ptr::write_volatile(self.common_cfg.add(COMMON_STATUS), val) }
    }

    pub fn set_status_bit(&self, bit: u8) {
        self.write_status(self.read_status() | bit);
    }

    pub fn read_device_features(&self) -> u64 {
        self.cfg_write_u32(COMMON_DFSELECT, 0);
        let low = self.cfg_read_u32(COMMON_DF);
        self.cfg_write_u32(COMMON_DFSELECT, 1);
        let high = self.cfg_read_u32(COMMON_DF);
        (high as u64) << 32 | low as u64
    }

    pub fn write_driver_features(&self, features: u64) {
        let low = features as u32;
        let high = (features >> 32) as u32;
        self.cfg_write_u32(COMMON_GFSELECT, 0);
        self.cfg_write_u32(COMMON_GF, low);
        self.cfg_write_u32(COMMON_GFSELECT, 1);
        self.cfg_write_u32(COMMON_GF, high);
    }

    pub fn reset(&self) {
        self.write_status(0);
        while self.read_status() != 0 {
            core::hint::spin_loop();
        }
    }

    pub fn select_queue(&self, index: u16) {
        self.cfg_write_u16(COMMON_QUEUE_SELECT, index);
    }

    pub fn queue_size(&self) -> u16 {
        self.cfg_read_u16(COMMON_QUEUE_SIZE)
    }

    pub fn set_queue_size(&self, size: u16) {
        self.cfg_write_u16(COMMON_QUEUE_SIZE, size);
    }

    pub fn set_queue_desc(&self, addr: u64) {
        self.cfg_write_u64(COMMON_QUEUE_DESC, addr);
    }

    pub fn set_queue_avail(&self, addr: u64) {
        self.cfg_write_u64(COMMON_QUEUE_AVAIL, addr);
    }

    pub fn set_queue_used(&self, addr: u64) {
        self.cfg_write_u64(COMMON_QUEUE_USED, addr);
    }

    pub fn enable_queue(&self) {
        self.cfg_write_u16(COMMON_QUEUE_ENABLE, 1);
    }

    /// Bind the selected queue to an MSI-X vector.
    ///
    /// virtio-pci raises no interrupt for a queue until this is written: the
    /// device starts every queue at `VIRTIO_MSI_NO_VECTOR` and stays silent,
    /// which reads exactly like a device that does not support interrupts at
    /// all. Select the queue first (`select_queue`), the way every other queue
    /// field here works. virtio 1.2 §4.1.4.3.
    ///
    /// Reads back what the device took: a device that has run out of vectors
    /// answers `VIRTIO_MSI_NO_VECTOR` rather than failing the write, so the
    /// only way to know it was accepted is to look.
    pub fn set_queue_msix_vector(&self, vector: u16) -> bool {
        self.cfg_write_u16(COMMON_QUEUE_MSIX, vector);
        self.cfg_read_u16(COMMON_QUEUE_MSIX) == vector
    }

    /// Bind device configuration-change notifications to an MSI-X vector.
    #[expect(unused)]
    pub fn set_config_msix_vector(&self, vector: u16) -> bool {
        self.cfg_write_u16(COMMON_MSIX_CONFIG, vector);
        self.cfg_read_u16(COMMON_MSIX_CONFIG) == vector
    }

    pub fn queue_notify_off(&self) -> u16 {
        self.cfg_read_u16(COMMON_QUEUE_NOTIFY_OFF)
    }

    #[expect(unused)]
    pub fn num_queues(&self) -> u16 {
        self.cfg_read_u16(COMMON_NUM_QUEUES)
    }

    /// Send a queue notification to the device.
    pub fn notify_queue(&self, queue_index: u16, notify_off: u16) {
        let addr =
            self.notify_base as usize + notify_off as usize * self.notify_off_multiplier as usize;
        unsafe {
            core::arch::asm!("mov [{1}], {0:x}", in(reg) queue_index, in(reg) addr, options(nostack))
        };
    }

    #[expect(unused)]
    pub fn device_cfg_ptr(&self) -> *mut u8 {
        self.device_cfg
    }

    // ---- Device initialisation sequence helpers ----

    /// Begin the virtio initialisation sequence (reset + ACKNOWLEDGE + DRIVER).
    pub fn init_device(&self) {
        self.reset();
        self.set_status_bit(VIRTIO_STATUS_ACKNOWLEDGE);
        self.set_status_bit(VIRTIO_STATUS_DRIVER);
    }

    /// Set FEATURES_OK and verify the device accepted the negotiated features.
    ///
    /// Panics if the device clears FEATURES_OK after the driver sets it,
    /// which means the feature set is not accepted.
    pub fn finish_init(&self) {
        self.set_status_bit(VIRTIO_STATUS_FEATURES_OK);
        if self.read_status() & VIRTIO_STATUS_FEATURES_OK == 0 {
            self.write_status(VIRTIO_STATUS_FAILED);
            panic!("virtio: device did not accept features");
        }
    }

    /// Complete initialisation by setting DRIVER_OK.
    pub fn set_driver_ok(&self) {
        self.set_status_bit(VIRTIO_STATUS_DRIVER_OK);
    }

    #[expect(unused)]
    pub fn pci_addr(&self) -> PciAddress {
        self.pci_addr
    }
}
