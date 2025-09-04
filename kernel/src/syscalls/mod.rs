use core::{
    arch::{asm, naked_asm},
    hint::black_box,
};

use x86_64::{
    VirtAddr,
    instructions::interrupts::enable_and_hlt,
    registers::{
        control::{Efer, EferFlags},
        model_specific::{GsBase, KernelGsBase, LStar, SFMask, Star},
        rflags::RFlags,
    },
};

use crate::{gdt::GDT, print, println, thread::scheduler::sched, util::per_cpu::get_percpu_data};

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

    println!("Kernel code: 0x{:x}", GDT.1.code_selector.0);
    println!("Kernel data: 0x{:x}", GDT.1.data_selector.0);
    println!("User code: 0x{:x}", GDT.1.user_code_selector.0);
    println!("User data: 0x{:x}", GDT.1.user_data_selector.0);

    // STAR register: set kernel/user code segments
    Star::write(
        GDT.1.user_code_selector,
        GDT.1.user_data_selector,
        GDT.1.code_selector,
        GDT.1.data_selector,
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

extern "C" fn syscall_handler(ctx: *mut SyscallContext) {
    let ctx = unsafe { ctx.as_mut().unwrap() };

    // Beware with some sched() calls, they call hlt which might hang if we don't have interrupts enabled.

    match ctx.rax {
        1 => {
            // sys_write
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *const u8;
            let count = ctx.rdx as usize;

            println!(
                "Syscall: sys_write(fd={}, buf={:p}, count={})",
                fd, buffer_ptr, count
            );

            if count == 0 {
                ctx.rax = 0;
                return;
            }

            // Read from user buffer
            let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, count) };

            // Convert to string (handle invalid UTF-8 gracefully)
            match core::str::from_utf8(buffer) {
                Ok(s) => {
                    println!("{}", s); // Use print! instead of println! to not add extra newline
                    ctx.rax = count as u64; // Return bytes written
                }
                Err(_) => {
                    // If not valid UTF-8, print as hex dump
                    println!(
                        "sys_write: Non-UTF8 data: {:02x?}",
                        &buffer[..count.min(64)]
                    );
                    ctx.rax = count as u64;
                }
            }
        }
        60 => {
            sched().thread_exit(ctx.rdi as i32);

            loop {
                enable_and_hlt();
            }
        }
        _ => {
            ctx.rax = !0u64;
        }
    }
}
