use spin::RwLock;
use x86_64::{PhysAddr, VirtAddr};

use crate::boot::boot_info;

pub mod cow;
pub mod fault;
pub mod frame_allocator;
pub mod mapper;
pub mod pat;
pub mod shared;
pub mod tlb;
pub mod valloc;
pub mod vma;

/// Allowlist of physical address ranges that userspace may map via MAP_PHYSICAL.
/// Each entry is (start, end) inclusive of start, exclusive of end.
static ALLOWED_PHYS_RANGES: RwLock<heapless::Vec<(u64, u64), 8>> =
    RwLock::new(heapless::Vec::new());

/// Register a physical address range as safe for userspace mapping.
pub fn allow_physical_range(start: u64, size: u64) {
    let mut ranges = ALLOWED_PHYS_RANGES.write();
    let _ = ranges.push((start, start + size));
}

/// Check whether the entire range [start, start+size) is within an allowed range.
pub fn is_physical_range_allowed(start: u64, size: u64) -> bool {
    let end = start + size;
    let ranges = ALLOWED_PHYS_RANGES.read();
    ranges
        .iter()
        .any(|&(r_start, r_end)| start >= r_start && end <= r_end)
}

// physical offset is at 0xffff_8000_0000_0000

pub const DYNAMIC_MEM_START: VirtAddr = VirtAddr::new_truncate(0xFFFF_C000_2000_0000);

// Early heap region
pub const KERNEL_HEAP: VirtAddr = VirtAddr::new_truncate(0xFFFF_C000_0000_0000);
// Ends at 0xFFFF_C000_1000_0000
pub const KERNEL_HEAP_SIZE: u64 = 1024 * 1024 * 1; // 1 mb

// Remember Debug rust builds take a lot of stack
pub const KTHREAD_STACK_SIZE: u64 = 1024 * 32; // 32kb
/// Size of stack region including guard page (total allocation per thread)
pub const KTHREAD_STACK_REGION_SIZE: u64 = KTHREAD_STACK_SIZE + 4096;

pub const USER_STACK_TOP: VirtAddr = VirtAddr::new_truncate(0x0000_7000_0000_0000);
pub const USER_STACK_SIZE: u64 = 1024 * 1024 * 8; // 8mb

/// Stack alignment requirement for FPU/SSE instructions (16 bytes)
pub const STACK_ALIGNMENT: u64 = 16;

/// Get the virtual address from the given physical address
///
/// This may not be mapped! check with translate
pub fn get_virt_addr_from_phys_offset(phys: PhysAddr) -> VirtAddr {
    boot_info().physical_memory_offset + phys.as_u64()
}
