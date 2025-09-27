use alloc::{collections::btree_map::BTreeMap, vec::Vec};
use core::{ptr, sync::atomic::AtomicU64};
use spin::Mutex;
use x86_64::{
    VirtAddr,
    registers::control::Cr3Flags,
    structures::paging::{PageTableFlags, PhysFrame},
};

use crate::{
    drivers::fpu::FpuState,
    fs::path::Path,
    memory::{STACK_ALIGNMENT, USER_STACK_SIZE, mapper::MemoryManager},
    syscalls::Errno,
    thread::fd::FileDescriptorTable,
};
use alloc::sync::Arc;

pub mod broadcast;
pub mod context;
pub mod fd;
pub mod interrupt;
pub mod mailbox;
pub mod paging;
pub mod pipe;
//pub mod scheduler;
pub mod irqlock;
pub mod mutex;
pub mod runqueue;
pub mod scheduler;
pub mod thread;
pub mod util;
pub mod waitqueue;

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
pub enum StackSetupError {
    StackOverflow,
}

pub fn setup_user_stack(
    stack_top: u64,
    argv: &[&[u8]],
) -> Result<(u64, u64, usize), StackSetupError> {
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
