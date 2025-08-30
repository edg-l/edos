use core::arch::naked_asm;

use crate::thread::scheduler::timer_schedule;

// Naked function for timer interrupt handler
#[unsafe(naked)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn timer_interrupt_handler() {
    naked_asm!(
        // CPU has already pushed SS, RSP, RFLAGS, CS, RIP
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
