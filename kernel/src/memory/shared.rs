//! Shared memory support for inter-process communication
//!
//! Provides shared memory regions that can be mapped into multiple process address spaces.

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use x86_64::structures::paging::{FrameAllocator, PhysFrame};

use crate::debug::lock_order::RANK_SHM_REGISTRY;
use crate::memory::frame_allocator::frame_allocator;
use crate::thread::preempt::PreemptRwLock;
use crate::{ranked_read, ranked_write};

/// Global registry of shared memory regions
pub static SHARED_MEMORY_REGISTRY: PreemptRwLock<BTreeMap<u64, Arc<SharedMemory>>> =
    PreemptRwLock::new(BTreeMap::new());

/// Counter for generating unique shared memory IDs
static NEXT_SHM_ID: AtomicU64 = AtomicU64::new(1);

/// A shared memory region that can be mapped into multiple address spaces
#[derive(Debug)]
pub struct SharedMemory {
    /// Physical frames backing this region
    frames: Vec<PhysFrame>,
    /// Size in bytes
    size: usize,
    /// Unique identifier
    id: u64,
    /// Number of active mappings
    ref_count: AtomicUsize,
    /// Marked for destruction (like Linux IPC_RMID); auto-removed when ref_count hits 0
    destroyed: AtomicBool,
}

#[derive(Debug)]
pub enum SharedMemoryError {
    /// Failed to allocate physical frames
    AllocationFailed,
    /// Shared memory region not found
    NotFound,
    /// Invalid size (zero or not page-aligned)
    InvalidSize,
    /// Region has been marked for destruction; no new mappings allowed
    Destroyed,
}

impl SharedMemory {
    /// Create a new shared memory region of the given size
    ///
    /// Size will be rounded up to the nearest page boundary.
    pub fn new(size: usize) -> Result<Arc<Self>, SharedMemoryError> {
        if size == 0 {
            return Err(SharedMemoryError::InvalidSize);
        }

        // Round up to page size
        let aligned_size = (size + 0xFFF) & !0xFFF;
        let frame_count = aligned_size / 4096;

        // Allocate physical frames in batches, releasing the IRQ lock between
        // batches so timer/device interrupts aren't starved during large SHM
        // allocations (e.g. 2048 frames for an 8MB framebuffer).
        const BATCH_SIZE: usize = 64;
        let mut frames = Vec::with_capacity(frame_count);
        while frames.len() < frame_count {
            let batch = (frame_count - frames.len()).min(BATCH_SIZE);
            let mut allocator = frame_allocator();
            for _ in 0..batch {
                let frame = allocator
                    .allocate_frame()
                    .ok_or(SharedMemoryError::AllocationFailed)?;
                frames.push(frame);
            }
            // Lock dropped here; IRQs re-enabled between batches.
        }

        // Zero the frames
        for frame in &frames {
            let virt = crate::memory::get_virt_addr_from_phys_offset(frame.start_address());
            unsafe {
                core::ptr::write_bytes(virt.as_mut_ptr::<u8>(), 0, 4096);
            }
        }

        let id = NEXT_SHM_ID.fetch_add(1, Ordering::Relaxed);

        let shm = Arc::new(Self {
            frames,
            size: aligned_size,
            id,
            ref_count: AtomicUsize::new(0),
            destroyed: AtomicBool::new(false),
        });

        // Register in global registry
        ranked_write!(RANK_SHM_REGISTRY, "shm::new", SHARED_MEMORY_REGISTRY)
            .insert(id, shm.clone());

        Ok(shm)
    }

    /// Get the shared memory ID
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Get the size in bytes
    pub fn size(&self) -> usize {
        self.size
    }

    /// Get the current reference count
    pub fn ref_count(&self) -> usize {
        self.ref_count.load(Ordering::Acquire)
    }

    /// Returns true if this region has been marked for destruction.
    pub fn is_destroyed(&self) -> bool {
        self.destroyed.load(Ordering::Acquire)
    }

    /// Increment the reference count.
    /// Returns error if the region is marked for destruction.
    pub fn inc_ref(&self) -> Result<(), SharedMemoryError> {
        // Increment first, then check destroyed to avoid TOCTOU race.
        self.ref_count.fetch_add(1, Ordering::AcqRel);
        if self.is_destroyed() {
            self.ref_count.fetch_sub(1, Ordering::AcqRel);
            return Err(SharedMemoryError::Destroyed);
        }
        Ok(())
    }

    /// Decrement the reference count.
    /// If ref_count reaches 0 and the entry was marked destroyed, removes it from the registry.
    pub fn dec_ref(&self) {
        let prev = self.ref_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 && self.destroyed.load(Ordering::Acquire) {
            ranked_write!(RANK_SHM_REGISTRY, "shm::dec_ref", SHARED_MEMORY_REGISTRY)
                .remove(&self.id);
        }
    }

    /// Get the physical frames backing this shared memory
    pub fn frames(&self) -> &[PhysFrame] {
        &self.frames
    }

    /// Look up a shared memory region by ID
    pub fn get(id: u64) -> Option<Arc<SharedMemory>> {
        ranked_read!(RANK_SHM_REGISTRY, "shm::get", SHARED_MEMORY_REGISTRY)
            .get(&id)
            .cloned()
    }

    /// Mark a shared memory region for destruction.
    ///
    /// If no active mappings remain, removes immediately from the registry.
    /// Otherwise, marks for deferred removal: the last `dec_ref()` that brings
    /// ref_count to 0 will remove the entry (like Linux `IPC_RMID`).
    pub fn destroy(id: u64) -> Result<(), SharedMemoryError> {
        let mut registry = ranked_write!(RANK_SHM_REGISTRY, "shm::destroy", SHARED_MEMORY_REGISTRY);
        let shm = registry.get(&id).ok_or(SharedMemoryError::NotFound)?;
        shm.destroyed.store(true, Ordering::Release);
        if shm.ref_count() > 0 {
            // Deferred: dec_ref() will clean up when ref_count reaches 0
            return Ok(());
        }
        registry.remove(&id);
        Ok(())
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // Deallocate physical frames in batches to avoid holding IRQ lock too long.
        const BATCH_SIZE: usize = 64;
        for chunk in self.frames.chunks(BATCH_SIZE) {
            let mut allocator = frame_allocator();
            for frame in chunk {
                unsafe {
                    allocator.deallocate_frame(*frame);
                }
            }
        }
        // Note: do NOT touch SHARED_MEMORY_REGISTRY here.
        // destroy() already removes the entry under the write lock,
        // and calling .write() again from Drop would deadlock (re-entrant).
    }
}
