// context.rs - CPU context structure definitions

#[repr(C)]
#[derive(Debug, Clone)]
pub struct CpuContext {
    // General purpose registers (saved by interrupt handler)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rax: u64,

    // Interrupt frame (pushed by CPU automatically)
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

impl CpuContext {
    pub const fn new() -> Self {
        CpuContext {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rdi: 0,
            rsi: 0,
            rbp: 0,
            rbx: 0,
            rdx: 0,
            rcx: 0,
            rax: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            rsp: 0,
            ss: 0,
        }
    }

    #[inline]
    pub fn is_from_userspace(&self) -> bool {
        // Check if CS register has RPL (Ring Privilege Level) of 3
        (self.cs & 0x3) == 3
    }

    #[inline]
    pub fn is_from_kernel(&self) -> bool {
        (self.cs & 0x3) == 0
    }
}

// timer.rs - Timer interrupt handler and scheduler

use core::arch::naked_asm;

use crate::{apic::LAPIC, gdt::GDT, serial_println};

// This function will be called from assembly with pointer to saved context
#[unsafe(no_mangle)]
pub extern "C" fn timer_schedule(context: *mut CpuContext) -> *mut CpuContext {
    unsafe {
        LAPIC.write().end_of_interrupt();

        let ctx = &mut *context;

        // Your scheduling logic here
        // Example: just print some info and return same context

        if ctx.is_from_userspace() {
            // Handle userspace -> kernel transition
            // Could switch to a different task here
            serial_println!("Context switch from user");
        } else {
            // Handle kernel -> kernel (nested interrupt or kernel task)
            serial_println!("Context switch from kernel");
        }

        // Return pointer to context to restore (could be different task)
        context
    }
}

// Naked function for timer interrupt handler
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn timer_interrupt_handler() {
    naked_asm!(
        // CPU has already pushed SS, RSP, RFLAGS, CS, RIP
        // We need to save all other general purpose registers

        // Save all general purpose registers
        "push rax",
        "push rcx",
        "push rdx",
        "push rbx",
        "push rbp",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // At this point, RSP points to the saved context
        // Pass it as first argument to timer_schedule
        "mov rdi, rsp",

        // Ensure stack is 16-byte aligned before call
        // The push operations above pushed 15 registers (8 bytes each = 120 bytes)
        // CPU pushed 5 values (40 bytes)
        // Total: 160 bytes, which is divisible by 16, so we're aligned

        // Clear direction flag as per x86-64 ABI
        "cld",

        // Call the Rust scheduler function
        "call {timer_schedule}",

        // RAX now contains pointer to context to restore (might be different task)
        // Move stack pointer to point to the context we want to restore
        "mov rsp, rax",

        // Restore all general purpose registers
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdi",
        "pop rsi",
        "pop rbp",
        "pop rbx",
        "pop rdx",
        "pop rcx",
        "pop rax",

        // Return from interrupt - will pop RIP, CS, RFLAGS, RSP, SS
        "iretq",

        timer_schedule = sym timer_schedule,
    );
}

// Optional: Helper functions for context manipulation

impl CpuContext {
    /// Initialize a context for a new kernel thread
    pub fn new_kernel_thread(entry_point: u64, stack_top: u64) -> Self {
        let mut ctx = Self::new();
        ctx.rip = entry_point;
        ctx.rsp = stack_top;
        ctx.cs = GDT.1.code_selector.0 as u64; // Kernel code segment (adjust for your GDT)
        ctx.ss = GDT.1.data_selector.0 as u64; // Kernel data segment (adjust for your GDT)
        ctx.rflags = 0x200; // Enable interrupts
        ctx
    }

    /// Initialize a context for a new user thread
    pub fn new_user_thread(entry_point: u64, stack_top: u64) -> Self {
        let mut ctx = Self::new();
        ctx.rip = entry_point;
        ctx.rsp = stack_top;
        ctx.cs = GDT.1.user_code_selector.0 as u64; // User code segment (0x20 | 3 for ring 3)
        ctx.ss = GDT.1.user_data_selector.0 as u64; // User data segment (0x18 | 3 for ring 3)
        ctx.rflags = 0x200; // Enable interrupts
        ctx
    }

    /// Switch from current context to this context
    /// Safety: This function never returns normally - execution continues from saved RIP
    pub unsafe fn switch_to(&self) -> ! {
        unsafe {
            core::arch::asm!(
                r#"
            # Load new stack pointer
            mov rsp, {ctx}

            # Restore all registers from context
            pop r15
            pop r14
            pop r13
            pop r12
            pop r11
            pop r10
            pop r9
            pop r8
            pop rdi
            pop rsi
            pop rbp
            pop rbx
            pop rdx
            pop rcx
            pop rax

            # Return to saved context
            iretq
            "#,
                ctx = in(reg) self as *const _ as u64,
                options(noreturn)
            )
        }
    }
}
