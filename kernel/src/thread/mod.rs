use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize};
use spin::Mutex;
use x86_64::{VirtAddr, registers::control::Cr3Flags, structures::paging::PhysFrame};

use crate::{
    fs::path::Path,
    loader::TlsTemplate,
    memory::{STACK_ALIGNMENT, USER_STACK_SIZE, mapper::MemoryManager, vma::VmaSet},
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
pub mod rwlock;
pub mod scheduler;
pub mod signal;
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
    pub vmas: Arc<spin::Mutex<VmaSet>>,
    pub tls: Option<UserThreadTls>,
    pub heap_break: u64,
    pub address_space_refs: Arc<AtomicUsize>,
    pub process_stack_top: Arc<AtomicU64>,
    /// Per-address-space TLS slot counter. Thread 0 uses slot 0, each
    /// subsequent clone'd thread gets the next slot via fetch_add.
    pub next_tls_slot: Arc<AtomicU64>,
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
    mm: &MemoryManager,
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
        mm.copy_to_user(VirtAddr::new(sp), &env[..len as usize]);
        mm.write_val_to_user::<u8>(VirtAddr::new(sp + len), 0);
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

        mm.copy_to_user(VirtAddr::new(sp), &arg[..len as usize]);
        mm.write_val_to_user::<u8>(VirtAddr::new(sp + len), 0);

        arg_ptrs.push(sp);
    }

    arg_ptrs.reverse();

    sp &= !(STACK_ALIGNMENT - 1);

    let argc = arg_ptrs.len();

    // CRITICAL: x86_64 System V ABI stack alignment for `_start`.
    //
    // The ABI requires RSP % 16 == 0 *before* a `call` instruction. After
    // `call` pushes the 8-byte return address, RSP % 16 == 8 inside the
    // callee. The callee's prologue (`push rbp`) then restores 16-alignment.
    //
    // `_start` is entered directly via iretq (no `call`), so we must set
    // RSP as if a `call` just happened: RSP % 16 == 8. If we get this
    // wrong, `_start`'s `push rbp` makes RSP % 16 == 0 instead of 8,
    // and the subsequent `call main` produces RSP % 16 == 8 inside main
    // instead of 0. The compiler emits `movaps` (requires 16-byte aligned
    // operands) for stack spills, which GPFs on the misaligned stack.
    //
    // Math: sp is 16-aligned here (from the & mask above). We push
    // total_pointers * 8 bytes below. After that:
    //   sp % 16 == (total_pointers % 2) * 8
    // We need sp % 16 == 8, so we add 8 bytes of padding when
    // total_pointers is EVEN (would give sp % 16 == 0 without padding).
    let total_pointers = 1 + argc + 1 + env_ptrs.len() + 1; // argc + argv ptrs + null + env ptrs + null
    if total_pointers % 2 == 0 {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        mm.write_val_to_user::<u64>(VirtAddr::new(sp), 0);
    }

    // Push null terminator for envp
    sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
    if sp < stack_bottom {
        return Err(StackSetupError::StackOverflow);
    }
    mm.write_val_to_user::<u64>(VirtAddr::new(sp), 0);

    // Push env pointers in reverse so first env is at lowest address
    for &ptr_value in env_ptrs.iter().rev() {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        mm.write_val_to_user::<u64>(VirtAddr::new(sp), ptr_value);
    }

    let envp_ptr = sp;

    // Push null terminator for argv
    sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
    if sp < stack_bottom {
        return Err(StackSetupError::StackOverflow);
    }
    mm.write_val_to_user::<u64>(VirtAddr::new(sp), 0);

    for &ptr_value in arg_ptrs.iter().rev() {
        sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        mm.write_val_to_user::<u64>(VirtAddr::new(sp), ptr_value);
    }

    let argv_ptr = sp;

    sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
    if sp < stack_bottom {
        return Err(StackSetupError::StackOverflow);
    }
    mm.write_val_to_user::<u64>(VirtAddr::new(sp), argc as u64);

    Ok((sp, argv_ptr, argc, envp_ptr))
}
