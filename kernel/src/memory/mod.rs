use x86_64::{PhysAddr, VirtAddr};

use crate::boot::boot_info;

pub mod frame_allocator;
pub mod mapper;
pub mod valloc;

pub const DYNAMIC_MEM_START: VirtAddr = VirtAddr::new_truncate(0xFFFF_C000_0000_0000);

pub const KERNEL_HEAP: VirtAddr = VirtAddr::new_truncate(0xFFFF_C940_0000_0000);
pub const KERNEL_HEAP_SIZE: u64 = 1024 * 1024 * 256; // 256 mb

// After heap
pub const ACPI_MAPPINGS: VirtAddr = VirtAddr::new_truncate(0xFFFF_C900_0800_0000);
// Assuming 1mb for acpi

pub const KTHREAD_STACK_FIRST: VirtAddr = VirtAddr::new_truncate(0xFFFF_C910_0000_0000);

// Remember Debug rust builds take a lot of stack
pub const KTHREAD_STACK_SIZE: u64 = 1024 * 32; // 32kb
/// Size of stack region including guard page (total allocation per thread)
pub const KTHREAD_STACK_REGION_SIZE: u64 = KTHREAD_STACK_SIZE + 4096;

pub const DMA_REGION_START: VirtAddr = VirtAddr::new_truncate(0xFFFF_DA00_0000_0000);

pub const USER_STACK_TOP: VirtAddr = VirtAddr::new_truncate(0x0000_7000_0000_0000);
pub const USER_STACK_SIZE: u64 = 1024 * 1024 * 8; // 8mb

/// Stack alignment requirement for FPU/SSE instructions (16 bytes)
pub const STACK_ALIGNMENT: u64 = 16;

/// Get the virtual address from the given physical address
pub fn get_virt_addr(phys: PhysAddr) -> VirtAddr {
    boot_info().physical_memory_offset + phys.as_u64()
}
