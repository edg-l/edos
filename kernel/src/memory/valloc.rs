//! Higher half virtual address allocator

use alloc::{collections::btree_map::BTreeMap, vec::Vec};
use spin::Mutex;
use x86_64::{VirtAddr, align_up};

use crate::memory::DYNAMIC_MEM_START;

static VALLOC: Mutex<VAlloc> = Mutex::new(VAlloc::new(DYNAMIC_MEM_START));

/// Virtual address allocator. Ensures there is a guard page at the end on each allocation.
struct VAlloc {
    pub next_start: VirtAddr,
    // size -> start
    pub freed: BTreeMap<u64, Vec<VirtAddr>>,
    pub alloc_sizes: BTreeMap<VirtAddr, u64>,
}

impl VAlloc {
    pub const fn new(start: VirtAddr) -> Self {
        Self {
            next_start: start,
            freed: BTreeMap::new(),
            alloc_sizes: BTreeMap::new(),
        }
    }

    pub fn alloc(&mut self, size: u64) -> VirtAddr {
        let size = align_up(size, 4096);
        if let Some(list) = self.freed.get_mut(&size)
            && let Some(x) = list.pop()
        {
            return x;
        }

        let start = self.next_start;
        self.next_start += size + 4096; // Add guard page
        self.alloc_sizes.insert(start, size);

        start
    }

    pub fn free(&mut self, start: VirtAddr) {
        if let Some(size) = self.alloc_sizes.get(&start) {
            let entry = self.freed.entry(*size).or_default();
            entry.push(start);
        }
    }
}

/// Returns a free virtual address.
///
/// All returned addresses are page aligned at start and end.
///
/// Guaranteed to have a guard page at the aligned end.
///
/// Note: you must map the address.
pub fn vmalloc(size: u64) -> VirtAddr {
    VALLOC.lock().alloc(size)
}

pub fn vfree(addr: VirtAddr) {
    VALLOC.lock().free(addr);
}
