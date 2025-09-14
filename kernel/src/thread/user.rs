use core::sync::atomic::AtomicU64;

use alloc::{collections::btree_map::BTreeMap, string::String, sync::Arc, vec::Vec};
use spin::Mutex;
use x86_64::{
    VirtAddr,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{OffsetPageTable, PageTableFlags, PhysFrame},
};

use crate::{
    boot::boot_info, drivers::fpu::FpuState, fs::path::Path, loader::{load_elf, ElfLoadError}, logs::ThreadLogger, memory::mapper::{active_level_4_table, get_level_4_table, MemoryManager}, println, smp::tlb_flush_all_including_global, syscalls::Errno, thread::{
        context::CpuContext, fd::FileDescriptorTable, paging::allocate_process_pml4, scheduler::switch_to_kernel_page, util::{kthread_stack_alloc, kthread_stack_free, thread_stack_alloc, thread_stack_free}, ThreadId, ThreadState
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
    pub memory_manager: Arc<Mutex<MemoryManager>>,
    pub memory_regions: Vec<MemoryRegion>,
    // Whether the fpu has been initialized for this thread.
    pub fpu_init: bool,
    pub fpu: FpuState,
    pub logger: Arc<ThreadLogger>,
    pub heap_break: u64,
}

#[derive(Debug)]
pub struct UserThreadInfo {
    pub errno: Errno,
    pub fd_table: FileDescriptorTable,
    // For mmap
    pub memory_mappings: BTreeMap<VirtAddr, MemoryMapping>,
    pub next_mmap_addr: VirtAddr,
    pub memory_manager: Arc<Mutex<MemoryManager>>,
    pub cwd: Path,
    pub user_id: u32,
    pub group_id: u32,
}

impl UserThreadInfo {
    pub fn from_thread(thread: &UserThread, user_id: u32, group_id: u32, cwd: Path) -> Self {
        Self {
            errno: Errno::Clear,
            fd_table: FileDescriptorTable::new(),
            memory_mappings: BTreeMap::new(),
            next_mmap_addr: VirtAddr::new(thread.heap_break),
            memory_manager: thread.memory_manager.clone(),
            cwd,
            user_id,
            group_id,
        }
    }
}

#[expect(unused)]
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

#[expect(unused)]
#[derive(Debug, Clone)]
pub struct MemoryMapping {
    pub size: u64,
    pub flags: PageTableFlags,
    pub mapping_type: MappingType, // Anonymous, File, etc.
}

#[derive(Debug, Clone)]
pub enum MappingType {
    Anonymous,
}

#[expect(unused)]
#[derive(Debug, Clone)]
pub struct File {
    fd: u64,
}

impl UserThread {
    /// Must provide entry point and cr3 page table.
    ///
    /// TODO: also handle arguments.
    ///
    /// Note: This function switches to kernel page, should be called without interrupts
    pub fn new(elf_data: &[u8], name: Option<String>) -> Result<Self, ElfLoadError> {
        switch_to_kernel_page();
        // allocate kernel stack before creating page
        let kernel_stack_top = kthread_stack_alloc();

        // Create user page.
        let kernel_pml4 = boot_info().cr3;
        let physical_memory_offset = boot_info().physical_memory_offset;
        let kernel_table = unsafe { get_level_4_table(kernel_pml4) };
        let page = unsafe { allocate_process_pml4(kernel_table) };

        // Use process page to set mappings
        unsafe { Cr3::write(page, kernel_pml4.1) };
        println!("Switched to new process page");

        let page_table = unsafe { active_level_4_table(physical_memory_offset) };
        let table = unsafe { OffsetPageTable::new(page_table, physical_memory_offset) };

        let mut process_memory_manager = MemoryManager::new(table);

        // call align
        let stack_top = thread_stack_alloc(&mut process_memory_manager);
        let stack_top_call_aligned = stack_top - 8;

        let load_info = load_elf(elf_data, &mut process_memory_manager)?;

        println!("loaded elf, back to kernel page");

        // Back to kernel page
        unsafe { Cr3::write(kernel_pml4.0, kernel_pml4.1) };

        println!("Creating CpuContext with entry_point: {:p}, stack_top: {:p}",
                load_info.entry_point.as_u64() as *const u8,
                stack_top_call_aligned as *const u8);

        let context =
            CpuContext::new_user_thread(load_info.entry_point.as_u64(), stack_top_call_aligned);

        static THREAD_NEXT_ID: AtomicU64 = AtomicU64::new(0);

        let id = THREAD_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        let name = Arc::new(name);

        let thread = UserThread {
            id: ThreadId::new_maybe_named(id, false, name.clone()),
            initial_stack_top: stack_top,
            context,
            state: ThreadState::Ready,
            kernel_stack_top,
            initial_kernel_stack_top: kernel_stack_top,
            cr3: (page, kernel_pml4.1),
            memory_manager: Arc::new(Mutex::new(process_memory_manager)),
            memory_regions: load_info.memory_regions,
            heap_break: load_info.heap_break,
            fpu_init: false,
            fpu: FpuState::default(),
            logger: Arc::new(ThreadLogger {
                id,
                kernel: false,
                name,
            }),
        };

        Ok(thread)
    }

    /// Cleans thread resources and switches to kernel page
    pub fn free(&mut self, info: Arc<Mutex<UserThreadInfo>>) {
        println!("Cleaning up thread resources");
        // Unmap all memory mappings
        let mut memory_manager = self.memory_manager.lock();
        for (&addr, mapping) in &info.lock().memory_mappings {
            let _ = memory_manager.unmap_memory(addr, mapping.size);
        }

        for region in &self.memory_regions {
            let _ = memory_manager.unmap_memory(region.start, region.size);
        }

        thread_stack_free(&mut memory_manager, self.initial_stack_top);
        kthread_stack_free(self.initial_kernel_stack_top);

        // clean up all page tables in the lower half of the address space
        memory_manager.clean_lower_half();
    }

    pub fn switch_to_page(&self) {
        if Cr3::read().0.start_address() != self.cr3.0.start_address() {
            unsafe { Cr3::write(self.cr3.0, self.cr3.1) };
        }
          tlb_flush_all_including_global();
    }
}
