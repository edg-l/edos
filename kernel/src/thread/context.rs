#![expect(unused)]

use x86_64::{
    PrivilegeLevel, VirtAddr, registers::rflags::RFlags, structures::idt::InterruptStackFrameValue,
};

use crate::gdt::GDT;

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
    pub interrupt_stack_frame: InterruptStackFrameValue,
}

impl CpuContext {
    pub const fn new(interrupt_stack_frame: InterruptStackFrameValue) -> Self {
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
            interrupt_stack_frame,
        }
    }

    #[inline]
    pub fn is_from_userspace(&self) -> bool {
        // Check if CS register has RPL (Ring Privilege Level) of 3
        self.interrupt_stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3
    }

    #[inline]
    pub fn is_from_kernel(&self) -> bool {
        self.interrupt_stack_frame.code_segment.rpl() == PrivilegeLevel::Ring0
    }
}

impl CpuContext {
    /// Initialize a context for a new kernel thread
    pub fn new_kernel_thread(entry_point: u64, stack_top: u64) -> Self {
        Self::new(InterruptStackFrameValue::new(
            VirtAddr::new(entry_point),
            GDT.1.code_selector,
            RFlags::INTERRUPT_FLAG,
            // Ensure stack is aligned after a "emulated" call.
            VirtAddr::new(stack_top - 8),
            GDT.1.data_selector,
        ))
    }

    /// Initialize a context for a new user thread
    pub fn new_user_thread(entry_point: u64, stack_top: u64) -> Self {
        Self::new(InterruptStackFrameValue::new(
            VirtAddr::new(entry_point),
            GDT.1.user_code_selector,
            RFlags::INTERRUPT_FLAG,
            VirtAddr::new(stack_top),
            GDT.1.user_data_selector,
        ))
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
