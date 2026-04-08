use core::sync::atomic::{Ordering, fence};

use crate::drivers::dma::{DmaBuffer, DmaError, dma};

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
/// Buffer is device-writable (used for responses written by the device).
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

/// A single descriptor in the virtqueue descriptor table.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

/// Split virtqueue with a descriptor table, available ring, and used ring.
///
/// The three regions are backed by a single contiguous DMA allocation laid out
/// as:
///   [descriptor table | available ring | (padding to 4 KiB page) | used ring]
///
/// All accesses to available/used ring headers and entries use volatile
/// reads/writes.  Memory ordering fences are placed at the same points the
/// virtio spec requires.
pub struct Virtqueue {
    /// DMA buffer backing the entire virtqueue.
    dma: DmaBuffer,
    /// Number of descriptors (power of two, max 128).
    size: u16,
    /// Pointer to the descriptor table.
    desc: *mut VirtqDesc,
    /// Pointer to the available ring (flags u16 | idx u16 | ring[size] u16 | used_event u16).
    avail: *mut u8,
    /// Pointer to the used ring (flags u16 | idx u16 | ring[size]*(id u32, len u32) | avail_event u16).
    used: *mut u8,
    /// Head of the free descriptor list.  `desc[free_head].next` is the next free.
    free_head: u16,
    /// Number of free descriptors remaining.
    num_free: u16,
    /// Index into the used ring of the last entry we have processed.
    last_used_idx: u16,
    /// Notify offset for this queue, as reported by the device.
    pub notify_off: u16,
}

unsafe impl Send for Virtqueue {}

impl Virtqueue {
    /// Allocate and initialise a split virtqueue with `size` descriptors.
    ///
    /// `size` must be a power of two and no greater than 128; if the device
    /// reports a smaller maximum the caller should use the minimum.
    pub fn new(size: u16) -> Result<Self, DmaError> {
        assert!(size > 0 && size <= 128, "virtqueue size must be 1..=128");

        // Section 2.7 layout:
        //   Descriptor table: size * 16 bytes
        //   Available ring:   4 + 2*size bytes   (flags u16 + idx u16 + ring[size] u16)
        //                     + 2 bytes used_event -> total 6 + 2*size
        //   Used ring:        4 + 8*size bytes   (flags u16 + idx u16 + ring[size]*(id u32, len u32))
        //                     + 2 bytes avail_event -> total 6 + 8*size
        //
        // The used ring must be aligned to 4 bytes; we align it to a 4096-byte
        // page boundary for simplicity (same as many drivers do).
        let desc_size = size as usize * 16;
        let avail_size = 6 + 2 * size as usize;

        // Pad between avail and used so used starts on a 4096-byte boundary.
        let avail_end = desc_size + avail_size;
        let used_offset = (avail_end + 0xFFF) & !0xFFF;
        let used_size = 6 + 8 * size as usize;
        let total = used_offset + used_size;

        let dma = dma().allocate_sized(total)?;

        let base = dma.virt_addr.as_u64() as *mut u8;

        let desc = base as *mut VirtqDesc;
        let avail = unsafe { base.add(desc_size) };
        let used = unsafe { base.add(used_offset) };

        // Initialise the free list: descriptor i points to i+1; the last
        // descriptor uses 0xFFFF as sentinel.
        for i in 0..size as usize {
            let d = unsafe { &mut *desc.add(i) };
            d.addr = 0;
            d.len = 0;
            d.flags = 0;
            d.next = if i + 1 < size as usize {
                (i + 1) as u16
            } else {
                0xFFFF
            };
        }

        // Avail ring: flags = 0, idx = 0 (already zeroed by DMA allocator, but be explicit)
        unsafe {
            core::ptr::write_volatile(avail as *mut u16, 0u16); // flags
            core::ptr::write_volatile((avail as *mut u16).add(1), 0u16); // idx
        }

        // Used ring: flags = 0, idx = 0
        unsafe {
            core::ptr::write_volatile(used as *mut u16, 0u16); // flags
            core::ptr::write_volatile((used as *mut u16).add(1), 0u16); // idx
        }

        Ok(Self {
            dma,
            size,
            desc,
            avail,
            used,
            free_head: 0,
            num_free: size,
            last_used_idx: 0,
            notify_off: 0,
        })
    }

    // ---- Physical address accessors ----

    pub fn desc_phys_addr(&self) -> u64 {
        let virt_offset = self.desc as u64 - self.dma.virt_addr.as_u64();
        self.dma.phys_addr().as_u64() + virt_offset
    }

    pub fn avail_phys_addr(&self) -> u64 {
        let virt_offset = self.avail as u64 - self.dma.virt_addr.as_u64();
        self.dma.phys_addr().as_u64() + virt_offset
    }

    pub fn used_phys_addr(&self) -> u64 {
        let virt_offset = self.used as u64 - self.dma.virt_addr.as_u64();
        self.dma.phys_addr().as_u64() + virt_offset
    }

    // ---- Descriptor submission ----

    /// Submit a descriptor chain to the device.
    ///
    /// Each entry in `bufs` is `(physical_address, length, flags)`.  The
    /// `VIRTQ_DESC_F_NEXT` flag is managed automatically; callers should pass
    /// `VIRTQ_DESC_F_WRITE` for device-writable buffers and 0 for
    /// driver-readable ones.
    ///
    /// Returns the head descriptor index on success, or `None` if there are
    /// not enough free descriptors.
    pub fn push(&mut self, bufs: &[(u64, u32, u16)]) -> Option<u16> {
        if bufs.is_empty() || bufs.len() as u16 > self.num_free {
            return None;
        }

        let head = self.free_head;

        // Walk through the buffers, filling in one free descriptor per entry.
        let mut cur = self.free_head;
        for (i, &(addr, len, flags)) in bufs.iter().enumerate() {
            let desc = unsafe { &mut *self.desc.add(cur as usize) };
            let next_free = desc.next; // save before we overwrite

            desc.addr = addr;
            desc.len = len;

            let is_last = i + 1 == bufs.len();
            if is_last {
                // Clear NEXT on the tail descriptor; preserve caller flags.
                desc.flags = flags & !VIRTQ_DESC_F_NEXT;
                // Advance free_head past the chain we consumed.
                self.free_head = next_free;
            } else {
                // Link this descriptor to the next one in the free list.
                desc.flags = flags | VIRTQ_DESC_F_NEXT;
                // desc.next already points to the next free descriptor,
                // which will be the next descriptor in the chain.
            }

            self.num_free -= 1;
            if !is_last {
                cur = next_free;
            }
        }

        // Make descriptor writes visible before we publish to the avail ring.
        fence(Ordering::Release);

        let avail_idx = self.read_avail_idx();
        let ring_slot = avail_idx % self.size;
        self.write_avail_ring(ring_slot, head);

        fence(Ordering::Release);
        self.write_avail_idx(avail_idx.wrapping_add(1));

        Some(head)
    }

    // ---- Completion polling ----

    /// Poll the used ring for a completed descriptor chain.
    ///
    /// Returns `(chain_head_id, bytes_written)` if the device has completed a
    /// request, or `None` if the used ring has no new entries.
    pub fn poll_used(&mut self) -> Option<(u16, u32)> {
        fence(Ordering::Acquire);
        let used_idx = self.read_used_idx();
        if self.last_used_idx == used_idx {
            return None;
        }

        let ring_slot = self.last_used_idx % self.size;
        let (id, len) = self.read_used_ring(ring_slot);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);

        Some((id as u16, len))
    }

    // ---- Descriptor reclaim ----

    /// Return a completed descriptor chain to the free list.
    ///
    /// `head` must be the chain head index returned by `push()`.
    pub fn reclaim(&mut self, head: u16) {
        let mut idx = head;
        loop {
            let desc = unsafe { &mut *self.desc.add(idx as usize) };
            let has_next = desc.flags & VIRTQ_DESC_F_NEXT != 0;
            let next = desc.next;

            // Prepend to free list.
            desc.next = self.free_head;
            desc.flags = 0;
            desc.addr = 0;
            desc.len = 0;
            self.free_head = idx;
            self.num_free += 1;

            if !has_next {
                break;
            }
            idx = next;
        }
    }

    // ---- Available ring helpers ----

    fn read_avail_idx(&self) -> u16 {
        unsafe { core::ptr::read_volatile((self.avail as *const u16).add(1)) }
    }

    fn write_avail_idx(&self, val: u16) {
        unsafe { core::ptr::write_volatile((self.avail as *mut u16).add(1), val) }
    }

    /// Write `desc_idx` into slot `ring_idx` of the available ring.
    ///
    /// The ring array starts at avail+4 (after flags u16 + idx u16).
    fn write_avail_ring(&self, ring_idx: u16, desc_idx: u16) {
        unsafe {
            core::ptr::write_volatile(
                (self.avail as *mut u16).add(2 + ring_idx as usize),
                desc_idx,
            )
        }
    }

    // ---- Used ring helpers ----

    fn read_used_idx(&self) -> u16 {
        unsafe { core::ptr::read_volatile((self.used as *const u16).add(1)) }
    }

    /// Read entry `ring_idx` from the used ring.
    ///
    /// Used ring layout: flags u16 | idx u16 | entries[size]*(id u32, len u32)
    /// So entries start at offset 4, each entry is 8 bytes.
    fn read_used_ring(&self, ring_idx: u16) -> (u32, u32) {
        let entry_ptr = unsafe { self.used.add(4 + ring_idx as usize * 8) };
        let id = unsafe { core::ptr::read_volatile(entry_ptr as *const u32) };
        let len = unsafe { core::ptr::read_volatile((entry_ptr as *const u32).add(1)) };
        (id, len)
    }
}
