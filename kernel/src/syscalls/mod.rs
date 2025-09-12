use core::arch::{asm, naked_asm};

use alloc::vec::Vec;
use x86_64::{
    VirtAddr,
    instructions::interrupts::enable_and_hlt,
    registers::{
        control::{Efer, EferFlags},
        model_specific::{GsBase, KernelGsBase, LStar, SFMask, Star},
        rflags::RFlags,
    },
};

use crate::{
    gdt::selectors,
    graphics::api::ScreenInfo,
    logs::LOG_BROADCAST,
    println,
    syscalls::{
        graphics::DrawRequestInput,
        io::{sys_close, sys_read, sys_write},
        keyboard::sys_keyboard_raw,
        memory::{sys_mmap, sys_munmap},
    },
    thread::scheduler::sched,
    util::per_cpu::get_percpu_data,
};

mod graphics;
mod io;
mod keyboard;
mod memory;

unsafe fn setup_gs_base() {
    let percpu = get_percpu_data();

    // Set GS base to point to per-CPU data
    let per_cpu_addr = &raw mut *percpu;
    GsBase::write(VirtAddr::new(per_cpu_addr as u64));
    KernelGsBase::write(VirtAddr::new(per_cpu_addr as u64));
}

pub fn set_gs_kernel_stack(stack: u64) {
    unsafe {
        asm! {
            "mov gs:8, {s}",
            s = in(reg) stack,
        }
    }
}

/// # Safety
/// Must be called once per core
pub unsafe fn setup_syscall() {
    unsafe {
        setup_gs_base();
    }

    let s = selectors();

    // STAR register: set kernel/user code segments
    Star::write(
        s.user_code_selector,
        s.user_data_selector,
        s.code_selector,
        s.data_selector,
    )
    .unwrap();

    // LSTAR: syscall entry point
    LStar::write(VirtAddr::new(syscall_entry as usize as u64));

    // SFMASK: flags to clear on syscall (clear interrupt flag for atomic entry)
    SFMask::write(RFlags::INTERRUPT_FLAG);

    let mut efer = Efer::read();
    efer |= EferFlags::SYSTEM_CALL_EXTENSIONS;
    unsafe { Efer::write(efer) };

    println!("SYSCALL/SYSRET enabled");
}

#[allow(unused)]
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    /*
        Summary for SYSCALL in x86-64 long mode:

        CPU saves (from user mode):
        RCX ⟵ user RIP (return address)
        R11 ⟵ user RFLAGS

        CPU loads (to kernel mode):

        RIP ⟵ IA32_LSTAR
        CS ⟵ IA32_STAR[47:32]
        SS ⟵ IA32_STAR[47:32] + 8

        RFLAGS ⟵ user_RFLAGS & ~IA32_FMASK (bits set in FMASK get cleared)

        Unchanged by hardware:

        RSP (you must set it)
        RAX, RBX, RDX, RSI, RDI, R8–R10, R12–R15
        FS/GS base registers (but you typically do SWAPGS)
    */
    naked_asm!(
        // Switch to kernel stack
        "swapgs",
        "mov gs:0, rsp",           // Save user RSP
        "mov rsp, gs:8",           // Load kernel stack

        // Build SyscallRegs structure on stack
        "push r11",                // rflags (RFLAGS saved by syscall)
        "push rcx",                // rip (RIP saved by syscall)

        "push rax",                // syscall number
        "push rdi",                // rdi (arg1)
        "push rsi",                // rsi (arg2)
        "push rdx",                // rdx (arg3)
        "push r8",                 // r8 (arg5)
        "push r9",                 // r9 (arg6)
        "push r10",                // r10 (arg4)
        "push rbx",                // rbx
        "push rbp",                // rbp
        "push r12",                // r12
        "push r13",                // r13
        "push r14",                // r14
        "push r15",                // r15

        // 15 * 8 = 120

        // Call handler with pointer to SyscallContext
        "mov rdi, rsp",            // Pass pointer to SyscallContext
        "call {handler}",

        // Restore all registers from SyscallContext structure
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rax",

        "pop rcx", // RIP
        "pop r11", // RFLAGS

        // Return to user
        "mov rsp, gs:0",
        "swapgs",
        "sysretq",

        handler = sym syscall_handler,
    );
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SyscallContext {
    // Saved in reverse order (last pushed = first in struct)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r10: u64, // arg4
    pub r9: u64,  // arg6
    pub r8: u64,  // arg5
    pub rdx: u64, // arg3
    pub rsi: u64, // arg2
    pub rdi: u64, // arg1
    pub rax: u64,
    pub rip: u64,    // User RIP
    pub rflags: u64, // User RFLAGS
}

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_PIPE: u64 = 22;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_EXIT: u64 = 60;
const SYS_ERRNO: u64 = 0x400;
const SYS_GETPID: u64 = 39; // get process ID
const SYS_DRAW_RECT: u64 = 100;
const SYS_RENDER: u64 = 101;
const SYS_SCREEN_INFO: u64 = 102;
const SYS_DRAW: u64 = 103;
const SYS_RAW_INPUT: u64 = 200;
const SYS_KERNEL_LOGS: u64 = 201;

extern "C" fn syscall_handler(ctx: *mut SyscallContext) {
    let ctx = unsafe { ctx.as_mut().unwrap() };

    // Beware with some sched() calls, they call hlt which might hang if we don't have interrupts enabled.

    // Note: we may need to call switch_to_kernel_page(); and switch back later.

    match ctx.rax {
        SYS_WRITE => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *const u8;
            let count = ctx.rdx as usize;
            ctx.rax = sys_write(fd, buffer_ptr, count);
        }
        SYS_READ => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *mut u8;
            let count = ctx.rdx as usize;
            ctx.rax = sys_read(fd, buffer_ptr, count) as u64;
        }
        SYS_RAW_INPUT => {
            let timeout = ctx.rdi;
            let buffer_ptr = ctx.rsi as *mut u32;
            let count = ctx.rdx as usize;
            ctx.rax = sys_keyboard_raw(timeout, buffer_ptr, count) as u64;
        }
        SYS_KERNEL_LOGS => {
            let buffer_ptr = ctx.rdi as *mut u8;
            let count = ctx.rsi as usize;
            ctx.rax = sys_kernel_log(buffer_ptr, count) as u64;
        }
        SYS_CLOSE => {
            let fd = ctx.rdi;
            ctx.rax = sys_close(fd) as u64;
        }
        SYS_MMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;
            let prot = ctx.rdx as u32;
            let flags = ctx.r10 as u32;

            ctx.rax = sys_mmap(addr, length, prot, flags);
        }
        SYS_MUNMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;

            ctx.rax = sys_munmap(addr, length) as u64;
        }
        SYS_EXIT => {
            sched().thread_exit(ctx.rdi as i32);

            loop {
                enable_and_hlt();
            }
        }
        SYS_GETPID => {
            ctx.rax = sys_getpid();
        }
        SYS_ERRNO => {
            ctx.rax = sys_errno();
        }
        SYS_DRAW_RECT => {
            ctx.rax = graphics::sys_draw_rect(ctx.rdi, ctx.rsi, ctx.rdx, ctx.r10, ctx.r8 as u32);
        }
        SYS_RENDER => {
            ctx.rax = graphics::sys_render();
        }
        SYS_SCREEN_INFO => {
            ctx.rax = graphics::sys_screen_info(ctx.rdi as *mut ScreenInfo);
        }
        SYS_DRAW => {
            ctx.rax = graphics::sys_draw(ctx.rdi as *const DrawRequestInput);
        }
        _ => {
            ctx.rax = !0u64;
        }
    }
}

pub fn sys_errno() -> u64 {
    let sched = sched();
    sched.current_thread_info().lock().errno as u64
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::upper_case_acronyms)]
pub enum Errno {
    Clear,
    EINVAL,
    ENOMEM,
    EFAULT,
}

fn sys_getpid() -> u64 {
    let sched = sched();
    let current_id = sched.current_id();
    current_id.id
}

// TODO: figure out why the syscall gets all logs. it doesnt properly subscribe?
pub fn sys_kernel_log(log_buffer: *mut u8, size: usize) -> i64 {
    let info = sched().current_thread_info();

    info.lock().errno = Errno::Clear;
    if log_buffer.is_null() {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let mut buf = Vec::with_capacity(size);

    // not needed?      x86_64::instructions::interrupts::enable();

    let rx = LOG_BROADCAST.lock().subscribe_or_get();

    // Require a 128 byte space.
    while buf.len() + 128 + 1 < size
        && let Some(log) = rx.try_recv()
    {
        let bytes = log.bytes();
        if buf.len() + bytes.len() + 1 < size {
            buf.extend(bytes);
            buf.push(b'\0');
        }
    }

    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), log_buffer, buf.len()) };

    buf.len() as i64
}
