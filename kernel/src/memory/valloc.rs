//! Higher half virtual address allocator.
//!
//! Uses a fixed-capacity sorted array of free regions. No heap allocation,
//! which is critical because the heap allocator calls vmalloc for expansion --
//! using heap-allocated structures here would create a circular dependency
//! that deadlocks when the heap is full.
//!
//! Neither path panics. Address space is a resource like any other, so
//! exhaustion is reported to the caller; `dma()` turns it into a `DmaError`
//! and the boot-time callers, which cannot proceed without their mapping,
//! say so at their own call site. A free that cannot be recorded is leaked
//! and counted rather than taken as fatal: a dying thread reaches `vfree`
//! through `Drop`, where a panic would be a second failure on top of the
//! first. `/proc/meminfo` carries the counters.

use core::sync::atomic::{AtomicU64, Ordering};

use x86_64::{VirtAddr, align_up};

use crate::{log, memory::DYNAMIC_MEM_START, thread::irqlock::IrqSpinlock};

/// Maximum number of free regions tracked. Each alloc can split one region
/// into two, so this limits concurrent live vmalloc regions to ~MAX_REGIONS.
/// 4096 entries × 16 bytes = 64 KiB of static memory.
const MAX_REGIONS: usize = 4096;

/// End of the vmalloc address range (kernel half, well below the direct map).
pub const VMALLOC_END: u64 = 0xFFFF_E000_0000_0000;

/// Regions dropped because the free list was full, and their bytes. Address
/// space is lost until reboot, which beats refusing to let a thread die.
static LEAKED_REGIONS: AtomicU64 = AtomicU64::new(0);
static LEAKED_BYTES: AtomicU64 = AtomicU64::new(0);

/// Frees naming a range that is already free. Each is a double free or a
/// bogus address, and recording it would hand one address to two owners.
static REJECTED_FREES: AtomicU64 = AtomicU64::new(0);

/// A snapshot of the allocator, for `/proc/meminfo`.
pub struct VallocStats {
    pub regions: usize,
    pub free_bytes: u64,
    pub largest_free: u64,
    pub leaked_regions: u64,
    pub leaked_bytes: u64,
    pub rejected_frees: u64,
}

#[derive(Clone, Copy)]
struct FreeRegion {
    start: u64,
    size: u64,
}

struct VAlloc {
    regions: [FreeRegion; MAX_REGIONS],
    count: usize,
}

impl VAlloc {
    const fn new(start: VirtAddr) -> Self {
        let total = VMALLOC_END - start.as_u64();
        let mut regions = [FreeRegion { start: 0, size: 0 }; MAX_REGIONS];
        regions[0] = FreeRegion {
            start: start.as_u64(),
            size: total,
        };
        Self { regions, count: 1 }
    }

    /// Allocate a page-aligned virtual region with a guard page after it, or
    /// `None` when no free region is large enough.
    fn alloc(&mut self, size: u64) -> Option<VirtAddr> {
        if size == 0 {
            return None;
        }
        let size = align_up(size, 4096);
        let need = size.checked_add(4096)?; // include guard page

        // First-fit scan.
        for i in 0..self.count {
            let r = self.regions[i];
            if r.size >= need {
                let result = r.start;

                let remaining = r.size - need;
                if remaining > 0 {
                    // Shrink this region (allocated portion is at the start).
                    self.regions[i] = FreeRegion {
                        start: r.start + need,
                        size: remaining,
                    };
                } else {
                    // Exact fit: remove this region.
                    self.remove(i);
                }

                return Some(VirtAddr::new(result));
            }
        }

        None
    }

    /// Free a region, merging with adjacent free regions.
    fn free(&mut self, addr: u64, size: u64) {
        let size = align_up(size, 4096);
        let region_size = size + 4096; // re-include the guard page
        let end = addr + region_size;

        // Find insertion point (regions are sorted by start address).
        let mut pos = self.count;
        for i in 0..self.count {
            if self.regions[i].start >= addr {
                pos = i;
                break;
            }
        }

        // A range that already overlaps free space is a double free or a
        // bogus address. Recording it would merge or duplicate an existing
        // region and a later `alloc` would hand one address to two owners,
        // so the range is dropped instead. Touching is not overlapping: the
        // merge cases below are `== addr` and `== end`.
        let overlaps_prev = pos > 0 && {
            let prev = self.regions[pos - 1];
            prev.start + prev.size > addr
        };
        let overlaps_next = pos < self.count && self.regions[pos].start < end;
        if overlaps_prev || overlaps_next {
            let n = REJECTED_FREES.fetch_add(1, Ordering::Relaxed);
            if n == 0 {
                log!("vmalloc: refused a free of an already-free range at {addr:#x}");
            }
            return;
        }

        // Try to merge with the previous region.
        let merge_prev = pos > 0 && {
            let prev = self.regions[pos - 1];
            prev.start + prev.size == addr
        };

        // Try to merge with the next region.
        let merge_next = pos < self.count && self.regions[pos].start == end;

        match (merge_prev, merge_next) {
            (true, true) => {
                // Merge prev + this + next into prev.
                let next_end = self.regions[pos].start + self.regions[pos].size;
                self.regions[pos - 1].size = next_end - self.regions[pos - 1].start;
                self.remove(pos);
            }
            (true, false) => {
                // Extend prev to include this region.
                self.regions[pos - 1].size += region_size;
            }
            (false, true) => {
                // Extend next backwards to include this region.
                self.regions[pos].start = addr;
                self.regions[pos].size += region_size;
            }
            (false, false) => {
                // A region touching neither neighbour needs a slot of its own.
                // With none left the address space is leaked: the caller is
                // often a `Drop` on a dying thread, which has no way to carry
                // the failure and nothing useful to do with it.
                if self.count == MAX_REGIONS {
                    let n = LEAKED_REGIONS.fetch_add(1, Ordering::Relaxed);
                    LEAKED_BYTES.fetch_add(region_size, Ordering::Relaxed);
                    if n == 0 {
                        log!(
                            "vmalloc: free list full at {MAX_REGIONS} regions, leaking {region_size} bytes at {addr:#x}"
                        );
                    }
                    return;
                }
                self.insert(
                    pos,
                    FreeRegion {
                        start: addr,
                        size: region_size,
                    },
                );
            }
        }
    }

    fn stats(&self) -> VallocStats {
        let mut free_bytes = 0;
        let mut largest_free = 0;
        for i in 0..self.count {
            free_bytes += self.regions[i].size;
            largest_free = largest_free.max(self.regions[i].size);
        }
        VallocStats {
            regions: self.count,
            free_bytes,
            largest_free,
            leaked_regions: LEAKED_REGIONS.load(Ordering::Relaxed),
            leaked_bytes: LEAKED_BYTES.load(Ordering::Relaxed),
            rejected_frees: REJECTED_FREES.load(Ordering::Relaxed),
        }
    }

    fn remove(&mut self, idx: usize) {
        for i in idx..self.count - 1 {
            self.regions[i] = self.regions[i + 1];
        }
        self.count -= 1;
    }

    /// Insert at `idx`. The caller has checked that a slot is free.
    fn insert(&mut self, idx: usize, region: FreeRegion) {
        debug_assert!(self.count < MAX_REGIONS);
        for i in (idx..self.count).rev() {
            self.regions[i + 1] = self.regions[i];
        }
        self.regions[idx] = region;
        self.count += 1;
    }
}

static VALLOC: IrqSpinlock<VAlloc> = IrqSpinlock::new(VAlloc::new(DYNAMIC_MEM_START));

/// Allocate virtual address space. Returns a page-aligned address with a
/// guard page after the region, or `None` when nothing free is large enough.
/// The caller must map physical pages.
pub fn vmalloc(size: u64) -> Option<VirtAddr> {
    VALLOC.lock().alloc(size)
}

/// Free virtual address space previously allocated with `vmalloc`.
/// `size` must match the size originally passed to `vmalloc`.
pub fn vfree(addr: VirtAddr, size: u64) {
    VALLOC.lock().free(addr.as_u64(), size)
}

/// A snapshot of the free list, for `/proc/meminfo`.
pub fn stats() -> VallocStats {
    VALLOC.lock().stats()
}
