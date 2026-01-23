//! Shared memory support for inter-process communication
//!
//! Provides shared memory regions that can be mapped into multiple process address spaces.

use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use spin::RwLock;
use x86_64::structures::paging::{FrameAllocator, PhysFrame};

use crate::memory::frame_allocator::frame_allocator;

/// Global registry of shared memory regions
pub static SHARED_MEMORY_REGISTRY: RwLock<BTreeMap<u64, Arc<SharedMemory>>> =
    RwLock::new(BTreeMap::new());

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
}

#[derive(Debug)]
pub enum SharedMemoryError {
    /// Failed to allocate physical frames
    AllocationFailed,
    /// Shared memory region not found
    NotFound,
    /// Invalid size (zero or not page-aligned)
    InvalidSize,
    /// Cannot destroy - still has active mappings
    StillMapped,
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

        // Allocate physical frames
        let mut frames = Vec::with_capacity(frame_count);
        {
            let mut allocator = frame_allocator();
            for _ in 0..frame_count {
                let frame = allocator
                    .allocate_frame()
                    .ok_or(SharedMemoryError::AllocationFailed)?;
                frames.push(frame);
            }
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
        });

        // Register in global registry
        SHARED_MEMORY_REGISTRY.write().insert(id, shm.clone());

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

    /// Increment the reference count
    pub fn inc_ref(&self) {
        self.ref_count.fetch_add(1, Ordering::AcqRel);
    }

    /// Decrement the reference count
    pub fn dec_ref(&self) {
        self.ref_count.fetch_sub(1, Ordering::AcqRel);
    }

    /// Get the physical frames backing this shared memory
    pub fn frames(&self) -> &[PhysFrame] {
        &self.frames
    }

    /// Look up a shared memory region by ID
    pub fn get(id: u64) -> Option<Arc<SharedMemory>> {
        SHARED_MEMORY_REGISTRY.read().get(&id).cloned()
    }

    /// Destroy a shared memory region (removes from registry)
    ///
    /// Returns error if there are still active mappings.
    pub fn destroy(id: u64) -> Result<(), SharedMemoryError> {
        let registry = SHARED_MEMORY_REGISTRY.read();
        if let Some(shm) = registry.get(&id) {
            if shm.ref_count() > 0 {
                return Err(SharedMemoryError::StillMapped);
            }
        } else {
            return Err(SharedMemoryError::NotFound);
        }
        drop(registry);

        // Remove from registry - this will drop the Arc
        // When the last Arc is dropped, the frames will be deallocated
        SHARED_MEMORY_REGISTRY.write().remove(&id);
        Ok(())
    }
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // Deallocate all physical frames
        let mut allocator = frame_allocator();
        for frame in &self.frames {
            unsafe {
                allocator.deallocate_frame(*frame);
            }
        }

        // Remove from registry if still present (defensive)
        SHARED_MEMORY_REGISTRY.write().remove(&self.id);
    }
}
