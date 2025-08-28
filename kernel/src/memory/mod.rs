use x86_64::{PhysAddr, VirtAddr};

use crate::boot::boot_info;

pub mod frame_allocator;
pub mod mapper;

pub const KERNEL_HEAP: VirtAddr = VirtAddr::new_truncate(0xFFFF_C900_0000_0000);
pub const KERNEL_HEAP_SIZE: u64 = 1024 * 1024 * 128; // 128 mb

#[expect(unused)]
pub const TEMP_MAPPINGS_START: VirtAddr = VirtAddr::new_truncate(0xFFFF_FFFE_0000_0000);

pub const ACPI_MAPPINGS: VirtAddr = VirtAddr::new_truncate(0xFFFF_FFF0_0000_0000);
pub const APIC_BASE: VirtAddr = VirtAddr::new_truncate(0xFFFF_FFF1_0000_0000);
pub const IOAPIC_BASE: VirtAddr = VirtAddr::new_truncate(0xFFFF_FFF2_0000_0000);

/// Stack alignment requirement for FPU/SSE instructions (16 bytes)
pub const STACK_ALIGNMENT: u64 = 16;

/// Get the virtual address from the given physical address
pub fn get_virt_addr(phys: PhysAddr) -> VirtAddr {
    boot_info().physical_memory_offset + phys.as_u64()
}
