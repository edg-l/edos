use x86_64::VirtAddr;

pub mod mapper;
pub mod frame_allocator;


pub const KERNEL_HEAP: VirtAddr = VirtAddr::new_truncate(0xFFFF_C900_0000_0000);
pub const KERNEL_HEAP_SIZE: u64 = 1024 * 1024 * 128; // 128 mb

#[expect(unused)]
pub const TEMP_MAPPINGS_START: VirtAddr = VirtAddr::new_truncate(0xFFFF_FFFE_0000_0000);


pub const ACPI_MAPPINGS: VirtAddr = VirtAddr::new_truncate(0xFFFF_FFF0_0000_0000);

/// Stack alignment requirement for FPU/SSE instructions (16 bytes)
pub const STACK_ALIGNMENT: u64 = 16;
