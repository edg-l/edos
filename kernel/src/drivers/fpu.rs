use bytemuck::{Pod, Zeroable};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

/// Enable FPU/SSE support
pub unsafe fn init_fpu() {
    unsafe {
        enable_fpu();
        enable_sse();
    }
}

/// Enable the FPU by clearing CR0.EM and setting CR0.MP
unsafe fn enable_fpu() {
    let mut cr0 = Cr0::read();
    cr0.remove(Cr0Flags::EMULATE_COPROCESSOR); // Clear EM bit
    cr0.insert(Cr0Flags::MONITOR_COPROCESSOR); // Set MP bit
    unsafe { Cr0::write(cr0) };
}

/// Enable SSE instructions
unsafe fn enable_sse() {
    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::OSFXSR); // Enable FXSAVE/FXRSTOR
    cr4.insert(Cr4Flags::OSXMMEXCPT_ENABLE); // Enable SSE exceptions
    unsafe { Cr4::write(cr4) };
}

/// Save FPU state using FXSAVE
pub unsafe fn save_fpu_state(state: &mut FpuState) {
    unsafe {
        core::arch::asm!(
            "fxsave [{}]",
            in(reg) state.data.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// Restore FPU state using FXRSTOR
pub unsafe fn restore_fpu_state(state: &FpuState) {
    unsafe {
        core::arch::asm!(
            "fxrstor [{}]",
            in(reg) state.data.as_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// Initialize FPU state for a new thread
pub unsafe fn init_fpu_state(state: &mut FpuState) {
    // Initialize with a clean FPU state
    unsafe {
        core::arch::asm!("fninit", options(nostack, preserves_flags));
        save_fpu_state(state);
    }
}

// FPU/SSE state structure (512 bytes for FXSAVE)
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct FpuState {
    data: [u8; 512],
}

impl Default for FpuState {
    fn default() -> Self {
        Self { data: [0; 512] }
    }
}
