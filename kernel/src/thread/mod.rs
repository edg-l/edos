use crate::thread::preempt::PreemptSpinlock;
use alloc::{string::String, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, AtomicUsize};
use x86_64::{VirtAddr, registers::control::Cr3Flags, structures::paging::PhysFrame};

use crate::{
    fs::path::Path,
    loader::TlsTemplate,
    memory::{STACK_ALIGNMENT, USER_STACK_SIZE, mapper::MemoryManager, vma::VmaSet},
    syscalls::Errno,
    thread::{fd::FileDescriptorTable, mutex::BlockingMutex},
};
pub mod broadcast;
pub mod cancel;
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
pub mod preempt;
pub mod runqueue;
pub mod rwlock;
pub mod sched_prof;
pub mod scheduler;
pub mod signal;
// `thread::thread` holds the Thread type itself; the parent module is the
// subsystem, not a re-export facade.
#[expect(
    clippy::module_inception,
    reason = "thread::thread is the Thread type itself, beside the scheduler that runs it"
)]
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
    pub memory_manager: Arc<PreemptSpinlock<MemoryManager>>,
    pub vmas: Arc<PreemptSpinlock<VmaSet>>,
    pub tls: Option<UserThreadTls>,
    pub heap_break: u64,
    pub address_space_refs: Arc<AtomicUsize>,
    pub process_stack_top: Arc<AtomicU64>,
    /// Per-address-space TLS slot counter. Thread 0 uses slot 0, each
    /// subsequent clone'd thread gets the next slot via fetch_add.
    pub next_tls_slot: Arc<AtomicU64>,
    /// The command line the image was started with, space-joined. Per address
    /// space, so `execve` replaces it along with everything else it installs.
    pub cmdline: Arc<String>,
}

#[derive(Debug, Clone)]
pub struct UserThreadTls {
    pub template: Arc<TlsTemplate>,
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
    pub memory_manager: Arc<PreemptSpinlock<MemoryManager>>,
    pub cwd: Arc<BlockingMutex<Path>>,
    pub user_id: u32,
    pub group_id: u32,
}

#[derive(Debug)]
pub enum StackSetupError {
    StackOverflow,
}

/// Auxiliary vector types the initial process stack carries
/// (System V x86-64 psABI §3.4.1).
mod auxv {
    pub const AT_NULL: u64 = 0;
    pub const AT_PHDR: u64 = 3;
    pub const AT_PHENT: u64 = 4;
    pub const AT_PHNUM: u64 = 5;
    pub const AT_PAGESZ: u64 = 6;
    pub const AT_BASE: u64 = 7;
    pub const AT_ENTRY: u64 = 9;
    pub const AT_SECURE: u64 = 23;
    pub const AT_RANDOM: u64 = 25;
    pub const AT_EXECFN: u64 = 31;

    /// Bytes of entropy `AT_RANDOM` points at. musl seeds its stack guard and
    /// its pointer mangling from exactly this many.
    pub const RANDOM_BYTES: usize = 16;

    /// Bytes reserved above `%fs` for the thread control block, of which the
    /// kernel uses only the self-pointer at offset 0.
    ///
    /// EDOS's own, in the range no psABI or Linux type reaches, since a runtime
    /// that reads someone else's vector must not find this by accident. It is
    /// what lets a runtime keeping state at a fixed `%fs` offset check that the
    /// offset is inside what the kernel actually reserved, and say so at start
    /// rather than run off the end of the mapping later.
    pub const AT_EDOS_TCB: u64 = 0x1000;
}

/// Where the image landed, as the auxiliary vector needs to describe it. Built
/// by the caller from the loader's result, because none of it is known until
/// the ELF has been parsed.
#[derive(Debug, Clone, Copy)]
pub struct ProcessAuxv {
    /// User address of the program header table, when the image maps one.
    pub phdr: Option<u64>,
    pub phentsize: u16,
    pub phnum: u16,
    /// The main image's entry point, relocated.
    pub entry: u64,
    /// Load base of the interpreter. Zero while nothing honours `PT_INTERP`,
    /// which is what a static image reports.
    pub base: u64,
}

pub fn setup_user_stack(
    stack_top: u64,
    argv: &[&[u8]],
    envp: &[&[u8]],
    execfn: &[u8],
    aux: &ProcessAuxv,
    mm: &MemoryManager,
) -> Result<(u64, u64, usize, u64), StackSetupError> {
    let stack_bottom = stack_top
        .checked_sub(USER_STACK_SIZE)
        .ok_or(StackSetupError::StackOverflow)?;

    let mut sp = stack_top;

    // Everything below descends the stack, so a push is a subtract then a
    // write. Both forms bounds-check against the stack VMA, since a stack that
    // does not fit its arguments must fail the load rather than write past it.
    let push_bytes = |sp: &mut u64, bytes: &[u8]| -> Result<u64, StackSetupError> {
        *sp = sp
            .checked_sub(bytes.len() as u64)
            .ok_or(StackSetupError::StackOverflow)?;
        if *sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        mm.copy_to_user(VirtAddr::new(*sp), bytes);
        Ok(*sp)
    };
    let push_cstr = |sp: &mut u64, bytes: &[u8]| -> Result<u64, StackSetupError> {
        // Terminator first: the stack descends, so the NUL has to be pushed
        // before the bytes it terminates in order to land above them.
        push_bytes(sp, &[0])?;
        push_bytes(sp, bytes)
    };
    let push_u64 = |sp: &mut u64, value: u64| -> Result<u64, StackSetupError> {
        *sp = sp.checked_sub(8).ok_or(StackSetupError::StackOverflow)?;
        if *sp < stack_bottom {
            return Err(StackSetupError::StackOverflow);
        }
        mm.write_val_to_user::<u64>(VirtAddr::new(*sp), value);
        Ok(*sp)
    };

    // AT_RANDOM and AT_EXECFN point into the string area, so they are laid down
    // with the strings rather than with the vector that names them.
    let mut random_bytes = [0u8; auxv::RANDOM_BYTES];
    crate::drivers::random::fill_bytes(&mut random_bytes);
    let random_ptr = push_bytes(&mut sp, &random_bytes)?;
    let execfn_ptr = push_cstr(&mut sp, execfn)?;

    // Push env strings (top of stack, reversed order)
    let mut env_ptrs = Vec::with_capacity(envp.len());
    for env in envp.iter().rev() {
        env_ptrs.push(push_cstr(&mut sp, env)?);
    }
    env_ptrs.reverse();

    // Push argv strings
    let mut arg_ptrs = Vec::with_capacity(argv.len());
    for arg in argv.iter().rev() {
        arg_ptrs.push(push_cstr(&mut sp, arg)?);
    }

    arg_ptrs.reverse();

    sp &= !(STACK_ALIGNMENT - 1);

    let argc = arg_ptrs.len();

    // The auxiliary vector, ascending, terminated by AT_NULL. A dynamic linker
    // has no other channel for AT_PHDR and AT_BASE, and musl will not start
    // without AT_RANDOM.
    let mut aux_entries: Vec<(u64, u64)> = Vec::with_capacity(10);
    if let Some(phdr) = aux.phdr {
        aux_entries.push((auxv::AT_PHDR, phdr));
        aux_entries.push((auxv::AT_PHENT, aux.phentsize as u64));
        aux_entries.push((auxv::AT_PHNUM, aux.phnum as u64));
    }
    aux_entries.push((auxv::AT_PAGESZ, crate::memory::vma::PAGE_SIZE));
    aux_entries.push((auxv::AT_BASE, aux.base));
    aux_entries.push((auxv::AT_ENTRY, aux.entry));
    aux_entries.push((auxv::AT_SECURE, 0));
    aux_entries.push((auxv::AT_RANDOM, random_ptr));
    aux_entries.push((auxv::AT_EXECFN, execfn_ptr));
    aux_entries.push((auxv::AT_EDOS_TCB, crate::thread::thread::TLS_TCB_MIN_SIZE));
    aux_entries.push((auxv::AT_NULL, 0));

    // Entry alignment. The psABI (§3.4.1) says `%rsp` is 16-byte aligned at
    // process entry, with `argc` at `[rsp]` — not the post-`call` state a
    // compiled function assumes, because entry is a jump and no return address
    // was pushed. An entry point that is an ordinary compiled function
    // therefore cannot be entered directly: its prologue would misalign by 8
    // and the first `movaps` spill would fault. Every libc's `crt1.o` masks
    // `%rsp` down before calling anything, and `_start` in the Rust fork is a
    // naked function that does the same, which is what lets this be correct
    // rather than accommodating.
    //
    // Math: sp is 16-aligned here (from the mask above) and total_words * 8
    // bytes go below it, leaving sp % 16 == (total_words % 2) * 8. Padding an
    // odd count with one more word brings it back to zero.
    // argc + argv ptrs + null + env ptrs + null + two words per auxv entry
    let total_words = 1 + argc + 1 + env_ptrs.len() + 1 + aux_entries.len() * 2;
    if total_words % 2 == 1 {
        push_u64(&mut sp, 0)?;
    }

    // Auxiliary vector, pushed back to front so AT_NULL ends up highest.
    for &(a_type, a_val) in aux_entries.iter().rev() {
        push_u64(&mut sp, a_val)?;
        push_u64(&mut sp, a_type)?;
    }

    // Push null terminator for envp
    push_u64(&mut sp, 0)?;

    // Push env pointers in reverse so first env is at lowest address
    for &ptr_value in env_ptrs.iter().rev() {
        push_u64(&mut sp, ptr_value)?;
    }

    let envp_ptr = sp;

    // Push null terminator for argv
    push_u64(&mut sp, 0)?;

    for &ptr_value in arg_ptrs.iter().rev() {
        push_u64(&mut sp, ptr_value)?;
    }

    let argv_ptr = sp;

    push_u64(&mut sp, argc as u64)?;

    Ok((sp, argv_ptr, argc, envp_ptr))
}
