#![expect(unused)]

use bytemuck::{Pod, Zeroable};
use x86_64::{
    PrivilegeLevel, VirtAddr, registers::rflags::RFlags, structures::idt::InterruptStackFrameValue,
};

use crate::gdt::selectors;

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

    #[inline]
    pub fn rip(&self) -> u64 {
        self.interrupt_stack_frame.instruction_pointer.as_u64()
    }
}

impl CpuContext {
    /// Initialize a context for a new kernel thread
    pub fn new_kernel_thread(entry_point: u64, stack_top: u64) -> Self {
        let s = selectors();
        Self::new(InterruptStackFrameValue::new(
            VirtAddr::new(entry_point),
            s.code_selector,
            RFlags::INTERRUPT_FLAG,
            // Ensure stack is aligned after a "emulated" call.
            VirtAddr::new(stack_top - 8),
            s.data_selector,
        ))
    }

    /// Initialize a context for a new user thread
    pub fn new_user_thread(entry_point: u64, stack_top: u64) -> Self {
        let s = selectors();
        Self::new(InterruptStackFrameValue::new(
            VirtAddr::new(entry_point),
            s.user_code_selector,
            RFlags::INTERRUPT_FLAG,
            VirtAddr::new(stack_top),
            s.user_data_selector,
        ))
    }
}
