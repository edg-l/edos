use alloc::{collections::btree_map::BTreeMap, sync::Arc, vec::Vec};
use core::{
    ptr,
    sync::atomic::{AtomicU64, AtomicUsize},
};
use spin::Mutex;
use x86_64::{
    VirtAddr,
    registers::control::Cr3Flags,
    structures::paging::{PageTableFlags, PhysFrame},
};

use crate::{
    fs::path::Path,
    loader::TlsTemplate,
    memory::{STACK_ALIGNMENT, USER_STACK_SIZE, mapper::MemoryManager},
    syscalls::Errno,
    thread::{fd::FileDescriptorTable, mutex::BlockingMutex},
};
pub mod broadcast;
pub mod context;
pub mod fd;
pub mod interrupt;
pub mod mailbox;
pub mod paging;
pub mod pipe;
pub mod pty;
//pub mod scheduler;
pub mod irqlock;
pub mod mutex;
pub mod poll;
pub mod runqueue;
pub mod scheduler;
pub mod thread;
pub mod util;
pub mod waitqueue;

#[cfg(feature = "sched-test")]
pub mod sched_test;

#[derive(Debug)]
pub struct UserThread {
    /// Same as thread id for now.
    pub pid: u64,
    /// Physical addr
    pub cr3: (PhysFrame, Cr3Flags),
    pub memory_manager: Arc<Mutex<MemoryManager>>,
    pub memory_regions: Arc<Vec<MemoryRegion>>,
    pub owned_regions: Vec<MemoryRegion>,
    pub tls: Option<UserThreadTls>,
    pub heap_break: u64,
    pub address_space_refs: Arc<AtomicUsize>,
    pub process_stack_top: Arc<AtomicU64>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UserThreadTls {
    pub template: Arc<TlsTemplate>,
    pub data_base: VirtAddr,
    pub data_size: u64,
    pub tcb_base: VirtAddr,
    pub tcb_size: u64,
    pub mapping_base: VirtAddr,
    pub mapping_size: u64,
}

/// Thread info, used for syscalls mainly, this struct is allowed to be freely modified by the thread itself at kernel level.
#[derive(Debug)]
pub struct UserThreadInfo {
    pub pid: u64,
    pub errno: Errno,
    pub fd_table: Arc<BlockingMutex<FileDescriptorTable>>,
    // For mmap
    pub memory_mappings: Arc<BlockingMutex<BTreeMap<VirtAddr, MemoryMapping>>>,
    pub next_mmap_addr: Arc<AtomicU64>,
    pub memory_manager: Arc<Mutex<MemoryManager>>,
    pub cwd: Arc<BlockingMutex<Path>>,
    pub user_id: u32,
    pub group_id: u32,
}

#[derive(Debug)]
pub enum StackSetupError {
    StackOverflow,
}

pub fn setup_user_stack(
    stack_top: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
) -> Result<(u64, u64, usize, u64), StackSetupError> {
    let stack_bottom = stack_top
        .checked_sub(USER_STACK_SIZE)
        .ok_or(StackSetupError::StackOverflow)?;

    let mut sp = stack_top;

    // Push env strings (top of stack, reversed order)
    let mut env_ptrs = Vec::with_capacity(envp.len());
    for env in envp.iter().rev() {
        let len = env.len() as u64;
        sp = sp
            .checked_sub(len + 1)
            .ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        unsafe {
            ptr::copy_nonoverlapping(env.as_ptr(), sp as *mut u8, len as usize);
            ((sp + len) as *mut u8).write(0);
        }
        env_ptrs.push(sp);
    }
    env_ptrs.reverse();

    // Push argv strings
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

    // x86_64 ABI: RSP must be 16-byte aligned *before* a `call` instruction.
    // Since `_start` is entered directly (not via `call`), we simulate the effect
    // of a `call` by making RSP ≡ 8 (mod 16) -- i.e., the "return address" slot
    // is implicit. After pushing all pointer slots (each 8 bytes), the final SP
    // must satisfy SP % 16 == 8.
    let total_pointers = 1 + argc + 1 + env_ptrs.len() + 1; // argc value + argv ptrs + null + env ptrs + null
    // sp is currently 16-aligned (from the & mask above).
    // After pushing total_pointers * 8 bytes, sp % 16 == (total_pointers % 2) * 8.
    // We want sp % 16 == 8, so pad when total_pointers is even (not odd).
    if total_pointers % 2 == 0 {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        unsafe { (sp as *mut u64).write(0) };
    }

    // Push null terminator for envp
    sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
    if sp < stack_bottom {
        return Err(StackSetupError::StackOverflow);
    }
    unsafe { (sp as *mut u64).write(0) };

    // Push env pointers in reverse so first env is at lowest address
    for &ptr_value in env_ptrs.iter().rev() {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        unsafe { (sp as *mut u64).write(ptr_value) };
    }

    let envp_ptr = sp;

    // Push null terminator for argv
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

    Ok((sp, argv_ptr, argc, envp_ptr))
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
    Tls,
    ThreadLocal,
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
    Shared(u64),   // shm_id
    Physical(u64), // physical base address (MMIO/VRAM, frames not owned by allocator)
}

#[expect(unused)]
#[derive(Debug, Clone)]
pub struct File {
    fd: u64,
}
