use core::sync::atomic::AtomicU64;

use alloc::vec::Vec;
use x86_64::{
    VirtAddr,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{OffsetPageTable, PageTableFlags, PhysFrame},
};

use crate::{
    boot::boot_info, loader::{load_elf, ElfLoadError}, memory::mapper::{active_level_4_table, get_level_4_table, MemoryManager}, println, thread::{
        context::CpuContext, paging::allocate_process_pml4, util::{kthread_stack_alloc, kthread_stack_free, thread_stack_alloc, thread_stack_free}, ThreadId, ThreadState
    }
};

#[derive(Debug)]
pub struct UserThread {
    pub id: ThreadId,
    /// Saved to free it in case the thread exits.
    pub initial_stack_top: u64,
    pub context: CpuContext,
    pub state: ThreadState,
    /// Physical addr
    pub cr3: (PhysFrame, Cr3Flags),
    pub initial_kernel_stack_top: u64,
    pub kernel_stack_top: u64,
    pub memory_manager: MemoryManager,
    pub memory_regions: Vec<MemoryRegion>,
}

#[derive(Debug, Clone)]
pub struct MemoryRegion {
    pub start: VirtAddr,
    pub size: u64,
    #[allow(unused)]
    pub flags: PageTableFlags,
    pub region_type: MemoryRegionType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemoryRegionType {
    Code,
    Data,
}

impl UserThread {
    /// Must provide entry point and cr3 page table.
    ///
    /// TODO: also handle arguments.
    ///
    /// Note: This must be called from the kernel cr3.
    pub fn new(elf_data: &[u8]) -> Result<Self, ElfLoadError> {
        // allocate kernel stack before creating page
        let kernel_stack_top = kthread_stack_alloc();

        // Create user page.
        let kernel_pml4 = boot_info().cr3;
        let physical_memory_offset = boot_info().physical_memory_offset;
        let kernel_table = unsafe { get_level_4_table(kernel_pml4) };
        let page = unsafe { allocate_process_pml4(kernel_table) };

        // Use process page to set mappings
        unsafe { Cr3::write(page, kernel_pml4.1) };

        let page_table = unsafe { active_level_4_table(physical_memory_offset) };
        let table = unsafe { OffsetPageTable::new(page_table, physical_memory_offset) };

        let mut process_memory_manager = MemoryManager::new(table);

        let stack_top = thread_stack_alloc();

        let load_info = load_elf(elf_data, &mut process_memory_manager)?;

        // Back to kernel page
        unsafe { Cr3::write(kernel_pml4.0, kernel_pml4.1) };

        let context = CpuContext::new_user_thread(load_info.entry_point.as_u64(), stack_top);

        static THREAD_NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let id = THREAD_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        let thread = UserThread {
            id: ThreadId::new(id, false),
            initial_stack_top: stack_top,
            context,
            state: ThreadState::Ready,
            kernel_stack_top,
            initial_kernel_stack_top: kernel_stack_top,
            cr3: (page, kernel_pml4.1),
            memory_manager: process_memory_manager,
            memory_regions: load_info.memory_regions,
        };

        println!("Created user thread: {:#?}", thread.initial_stack_top);

        Ok(thread)
    }

    pub fn free(&self) {
        // todo: memory cleanup
        // todo: cleanup page table and switch to kernel
        thread_stack_free(self.initial_stack_top);
        kthread_stack_free(self.initial_kernel_stack_top);
    }
}
