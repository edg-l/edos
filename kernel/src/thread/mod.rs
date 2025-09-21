use alloc::{collections::btree_map::BTreeMap, vec::Vec};
use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicU64},
    time::Duration,
};
use spin::Mutex;
use x86_64::{
    VirtAddr,
    registers::control::{Cr3, Cr3Flags},
    structures::paging::{OffsetPageTable, PageTableFlags, PhysFrame},
};

use crate::{
    boot::boot_info,
    drivers::fpu::FpuState,
    fs::path::Path,
    loader::{ElfLoadError, load_elf},
    memory::{
        STACK_ALIGNMENT, USER_STACK_SIZE,
        mapper::{MemoryManager, active_level_4_table, get_level_4_table},
    },
    println,
    syscalls::Errno,
    thread::{
        fd::FileDescriptorTable,
        paging::allocate_process_pml4,
        scheduler::switch_to_kernel_page,
        util::{thread_stack_alloc, thread_stack_free},
    },
};
use alloc::{string::String, sync::Arc};

use crate::{
    logs::ThreadLogger,
    thread::{
        context::CpuContext,
        util::{kthread_stack_alloc, kthread_stack_free},
    },
    timer::Instant,
};

pub mod broadcast;
pub mod context;
pub mod fd;
pub mod interrupt;
pub mod mailbox;
pub mod paging;
pub mod pipe;
pub mod scheduler;
pub mod util;

#[derive(Debug, Clone, PartialEq, PartialOrd, Eq, Ord)]
pub enum ThreadState {
    Ready,
    Waiting,
    WaitTimeout((Instant, Duration)),
    Exited(i32),
}

#[derive(Debug)]
pub struct Thread {
    // internal thread id
    pub id: u64,
    pub name: Arc<Option<String>>,
    /// Saved to free it in case the thread exits.
    pub initial_kstack_top: u64,
    pub context: CpuContext,
    pub state: ThreadState,
    pub logger: Arc<ThreadLogger>,
    /// Some if it's a user thread.
    pub user: Option<UserThread>,
    pub is_queued: AtomicBool,
}

#[derive(Debug)]
pub struct UserThread {
    /// Same as thread if for now.
    pub pid: u64,
    /// Saved to free it in case the thread exits.
    pub initial_stack_top: u64,
    /// Physical addr
    pub cr3: (PhysFrame, Cr3Flags),
    pub memory_manager: Arc<Mutex<MemoryManager>>,
    pub memory_regions: Vec<MemoryRegion>,
    // Whether the fpu has been initialized for this thread.
    pub fpu_init: bool,
    pub fpu: FpuState,
    pub heap_break: u64,
}

/// Thread info, used for syscalls mainly, this struct is allowed to be freely modified by the thread itself at kernel level.
#[derive(Debug)]
pub struct UserThreadInfo {
    pub pid: u64,
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
            pid: thread.pid,
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

#[derive(Debug)]
enum StackSetupError {
    StackOverflow,
}

fn setup_user_stack(stack_top: u64, argv: &[&[u8]]) -> Result<(u64, u64, usize), StackSetupError> {
    let stack_bottom = stack_top
        .checked_sub(USER_STACK_SIZE)
        .ok_or(StackSetupError::StackOverflow)?;

    let mut sp = stack_top;
    let mut arg_ptrs = Vec::with_capacity(argv.len());

    for arg in argv.iter().rev() {
        let len = arg.len() as u64;
        sp = sp
            .checked_sub(len + 1)
            .ok_or(StackSetupError::StackOverflow)?;

        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }

        unsafe {
            ptr::copy_nonoverlapping(arg.as_ptr(), sp as *mut u8, len as usize);
            ((sp + len) as *mut u8).write(0);
        }

        arg_ptrs.push(sp);
    }

    arg_ptrs.reverse();

    sp &= !(STACK_ALIGNMENT - 1);

    let argc = arg_ptrs.len();

    if argc % 2 == 0 {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        unsafe { (sp as *mut u64).write(0) };
    }

    sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
    if sp < stack_bottom {
        return Err(StackSetupError::StackOverflow);
    }
    unsafe { (sp as *mut u64).write(0) };

    for &ptr_value in arg_ptrs.iter().rev() {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        unsafe { (sp as *mut u64).write(ptr_value) };
    }

    let argv_ptr = sp;

    sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
    if sp < stack_bottom {
        return Err(StackSetupError::StackOverflow);
    }
    unsafe { (sp as *mut u64).write(argc as u64) };

    Ok((sp, argv_ptr, argc))
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

// For now kernel threads and user share id
static THREAD_ID_NEXT_ID: AtomicU64 = AtomicU64::new(0);

impl Thread {
    pub fn new_kernel(name: Option<String>, entry_point: u64) -> Self {
        let initial_kstack_top = kthread_stack_alloc();
        let context =
            CpuContext::new_kernel_thread(entry_point as *const u8 as u64, initial_kstack_top);

        let id = THREAD_ID_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        let name = Arc::new(name);

        Thread {
            id,
            initial_kstack_top,
            context,
            state: ThreadState::Ready,
            logger: Arc::new(ThreadLogger {
                kernel: true,
                id,
                name: name.clone(),
            }),
            name,
            user: None,
            is_queued: AtomicBool::new(false),
        }
    }

    /// Must provide entry point and cr3 page table.
    ///
    /// Note: This function switches to kernel page, should be called without interrupts
    pub fn new_user(
        elf_data: &[u8],
        name: Option<String>,
        argv: &[&[u8]],
    ) -> Result<Self, ElfLoadError> {
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

        let (user_stack_pointer, argv_ptr, argc) =
            setup_user_stack(stack_top, argv).map_err(|_| ElfLoadError::MappingFailed)?;

        let load_info = load_elf(elf_data, &mut process_memory_manager)?;

        println!("loaded elf, back to kernel page");

        // Back to kernel page
        unsafe { Cr3::write(kernel_pml4.0, kernel_pml4.1) };

        println!(
            "Creating CpuContext with entry_point: {:p}, stack_top: {:p}",
            load_info.entry_point.as_u64() as *const u8,
            user_stack_pointer as *const u8
        );

        let mut context =
            CpuContext::new_user_thread(load_info.entry_point.as_u64(), user_stack_pointer);
        context.rdi = argc as u64;
        context.rsi = argv_ptr;
        context.rdx = 0;

        let id = THREAD_ID_NEXT_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        let name = Arc::new(name);

        let thread = Thread {
            id,
            initial_kstack_top: kernel_stack_top,
            context,
            state: ThreadState::Ready,
            logger: Arc::new(ThreadLogger {
                kernel: false,
                id,
                name: name.clone(),
            }),
            name: name.clone(),
            user: Some(UserThread {
                pid: id,
                initial_stack_top: stack_top,
                cr3: (page, kernel_pml4.1),
                memory_manager: Arc::new(Mutex::new(process_memory_manager)),
                memory_regions: load_info.memory_regions,
                heap_break: load_info.heap_break,
                fpu_init: false,
                fpu: FpuState::default(),
            }),
            is_queued: AtomicBool::new(false),
        };

        Ok(thread)
    }

    pub fn free(&self, info: Option<Arc<Mutex<UserThreadInfo>>>) {
        kthread_stack_free(self.initial_kstack_top);

        if let Some(user) = &self.user {
            // Unmap all memory mappings
            let mut memory_manager = user.memory_manager.lock();
            if let Some(info) = info {
                for (&addr, mapping) in &info.lock().memory_mappings {
                    let _ = memory_manager.unmap_memory(addr, mapping.size);
                }
            }

            for region in &user.memory_regions {
                let _ = memory_manager.unmap_memory(region.start, region.size);
            }

            thread_stack_free(&mut memory_manager, user.initial_stack_top);

            // clean up all page tables in the lower half of the address space
            memory_manager.clean_lower_half();
        }
    }

    pub fn switch_to_page(&self) {
        if let Some(user) = &self.user {
            if Cr3::read().0.start_address() != user.cr3.0.start_address() {
                unsafe { Cr3::write(user.cr3.0, user.cr3.1) };
            }
            //tlb_flush_all_including_global();
        } else {
            switch_to_kernel_page();
        }
    }
}
