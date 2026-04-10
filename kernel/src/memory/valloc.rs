//! Higher half virtual address allocator.
//!
//! Uses a fixed-capacity sorted array of free regions. No heap allocation,
//! which is critical because the heap allocator calls vmalloc for expansion --
//! using heap-allocated structures here would create a circular dependency
//! that deadlocks when the heap is full.

use x86_64::{VirtAddr, align_up};

use crate::{memory::DYNAMIC_MEM_START, thread::irqlock::IrqSpinlock};

/// Maximum number of free regions tracked. Each alloc can split one region
/// into two, so this limits concurrent live vmalloc regions to ~MAX_REGIONS.
/// 4096 entries × 16 bytes = 64 KiB of static memory.
const MAX_REGIONS: usize = 4096;

/// End of the vmalloc address range (kernel half, well below the direct map).
pub const VMALLOC_END: u64 = 0xFFFF_E000_0000_0000;

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

    /// Allocate a page-aligned virtual region with a guard page after it.
    fn alloc(&mut self, size: u64) -> VirtAddr {
        let size = align_up(size, 4096);
        let need = size + 4096; // include guard page

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

                return VirtAddr::new(result);
            }
        }

        panic!("vmalloc: out of virtual address space");
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
                // Insert new free region.
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

    fn remove(&mut self, idx: usize) {
        for i in idx..self.count - 1 {
            self.regions[i] = self.regions[i + 1];
        }
        self.count -= 1;
    }

    fn insert(&mut self, idx: usize, region: FreeRegion) {
        assert!(
            self.count < MAX_REGIONS,
            "vmalloc: too many free regions ({})",
            MAX_REGIONS
        );
        for i in (idx..self.count).rev() {
            self.regions[i + 1] = self.regions[i];
        }
        self.regions[idx] = region;
        self.count += 1;
    }
}

static VALLOC: IrqSpinlock<VAlloc> = IrqSpinlock::new(VAlloc::new(DYNAMIC_MEM_START));

/// Allocate virtual address space. Returns a page-aligned address with a
/// guard page after the region. The caller must map physical pages.
pub fn vmalloc(size: u64) -> VirtAddr {
    VALLOC.lock().alloc(size)
}

/// Free virtual address space previously allocated with `vmalloc`.
/// `size` must match the size originally passed to `vmalloc`.
pub fn vfree(addr: VirtAddr, size: u64) {
    VALLOC.lock().free(addr.as_u64(), size)
}
