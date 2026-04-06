use x86_64::{PhysAddr, VirtAddr};

use crate::boot::boot_info;

pub mod frame_allocator;
pub mod mapper;
pub mod pat;
pub mod shared;
pub mod valloc;

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
