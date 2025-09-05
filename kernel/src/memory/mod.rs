use x86_64::{PhysAddr, VirtAddr};

use crate::boot::boot_info;

pub mod frame_allocator;
pub mod mapper;

pub const KERNEL_HEAP: VirtAddr = VirtAddr::new_truncate(0xFFFF_C900_0000_0000);
pub const KERNEL_HEAP_SIZE: u64 = 1024 * 1024 * 128; // 128 mb

// After heap
pub const ACPI_MAPPINGS: VirtAddr = VirtAddr::new_truncate(0xFFFF_C900_0800_0000);
// Assuming 1mb for acpi

pub const KTHREAD_STACK_FIRST: VirtAddr = VirtAddr::new_truncate(0xFFFF_C900_1000_0000);
pub const KTHREAD_STACK_SIZE: u64 = 4096 * 8;

pub const USER_STACK_TOP: VirtAddr = VirtAddr::new_truncate(0x0000_7000_0000_0000);
pub const USER_STACK_SIZE: u64 = 4096 * 2;

/// Stack alignment requirement for FPU/SSE instructions (16 bytes)
pub const STACK_ALIGNMENT: u64 = 16;

/// Get the virtual address from the given physical address
pub fn get_virt_addr(phys: PhysAddr) -> VirtAddr {
    boot_info().physical_memory_offset + phys.as_u64()
}
