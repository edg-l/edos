use core::arch::naked_asm;

use crate::thread::context::restore_context_and_iretq;

use crate::{
    apic::get_lapic,
    thread::{
        context::CpuContext,
        scheduler::{tick_finish, tick_prepare},
    },
};

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

        // The push operations above pushed 15 registers (8 bytes each = 120 bytes)
        // CPU pushed 5 values (40 bytes)
        // Total: 160 bytes, which is divisible by 16, we may be aligned but initial rsp might not

        // r12 holds the context across both phases. It is callee-saved, so it
        // survives the Rust calls, and its original value is already in the
        // frame the epilogue pops from.
        "mov r12, rsp",

        "mov rdi, r12",
        // Ensure stack is 16-byte aligned before call
        "sub rsp, 8",
        "and rsp, -16",
        // Clear direction flag as per x86-64 ABI
        "cld",
        "call {tick_prepare}",

        // rax = per-CPU scheduler stack to pivot to, or 0 to stay put.
        "mov r13, rax",
        "test rax, rax",
        "jz .Ltick_no_pivot",

        // Phase 1 saved the outgoing thread's context, so phase 2 is about to
        // publish it and another CPU may then resume it — on the very stack
        // this frame sits on. Copy the frame to the per-CPU scheduler stack
        // and run the rest of the tick, including the iretq, from there.
        "mov rsp, rax",
        "sub rsp, 160",
        "mov rdi, rsp",
        "mov rsi, r12",
        "mov ecx, 20",
        "cld",
        "rep movsq",
        "mov r12, rsp",

        ".Ltick_no_pivot:",
        "mov rdi, r12",
        "mov rsi, r13",
        "sub rsp, 8",
        "and rsp, -16",
        "cld",
        "call {tick_finish}",

        // RAX now contains the context to restore, which may be a different
        // thread's.
        restore_context_and_iretq!(),

        tick_prepare = sym timer_tick_prepare,
        tick_finish = sym timer_tick_finish,
    );
}

/// First half of a timer tick, still on the interrupted thread's stack.
///
/// Returns the per-CPU scheduler stack for the caller to pivot to, or 0 to stay
/// put. See `Scheduler::tick_prepare` for why leaving matters.
#[unsafe(no_mangle)]
pub extern "C" fn timer_tick_prepare(context: *mut CpuContext) -> u64 {
    // SAFETY: an EOI write to this CPU's own LAPIC, from the handler of the
    // vector it acknowledges. The LAPIC is mapped and enabled before any
    // interrupt can be delivered, so the register exists by the time this runs.
    unsafe { get_lapic().end_of_interrupt() };
    tick_prepare(context)
}

/// Second half, on the per-CPU scheduler stack when phase 1 asked for a pivot.
#[unsafe(no_mangle)]
pub extern "C" fn timer_tick_finish(context: *mut CpuContext, pivoted: u64) -> *mut CpuContext {
    tick_finish(context, pivoted != 0);
    context
}
