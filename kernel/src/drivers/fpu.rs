//! FPU/SSE state, and how a context switch carries it.
//!
//! The kernel itself is built `-sse,+soft-float` and never touches an FPU or
//! vector register, so the only state that has to survive a switch belongs to
//! user threads, and nothing disturbs it between a thread being switched out
//! and switched back in.
//!
//! `FXSAVE`/`FXRSTOR` is the whole mechanism, and `XSAVEOPT` was tried and is
//! not worth it here — see the note above [`save_fpu_state`].

use bytemuck::{Pod, Zeroable};
use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};

use crate::println;

/// Enable FPU/SSE support.
///
/// # Safety
/// Runs on the CPU whose control registers it writes, so the caller must not
/// be migratable across it, and it must run once per CPU during bring-up,
/// before anything on that CPU saves, restores or initialises FPU state.
pub unsafe fn init_fpu() {
    // SAFETY: the two helpers below inherit this function's own contract, and
    // `emms` only clears the x87 tag word on the CPU being brought up, which
    // has no FPU state of anyone's to lose yet.
    unsafe {
        enable_fpu();
        enable_sse();
        println!("Enabled FPU and SSE");
        // Simple MMX instruction test
        core::arch::asm!(
            "emms", // Empty MMX state
            options(nostack, preserves_flags)
        );
        println!("MMX test passed");
    }
}

/// Enable the FPU by clearing CR0.EM and setting CR0.MP
///
/// # Safety
/// Writes CR0 on the calling CPU, so the caller must not be migratable, and
/// no FPU instruction may be in flight on it.
unsafe fn enable_fpu() {
    let mut cr0 = Cr0::read();
    cr0.remove(Cr0Flags::EMULATE_COPROCESSOR); // Clear EM bit
    cr0.insert(Cr0Flags::MONITOR_COPROCESSOR); // Set MP bit
    // SAFETY: EM and MP decide whether x87 traps or executes; the kernel is
    // built `-sse,+soft-float` and issues no FPU instruction of its own, so no
    // in-flight state depends on the value being replaced. Every other CR0 bit
    // is carried over from the read above.
    unsafe { Cr0::write(cr0) };
}

/// Enable SSE instructions
///
/// # Safety
/// Writes CR4 on the calling CPU, so the caller must not be migratable.
/// [`enable_fpu`] must have run first: `OSFXSR` promises the OS saves and
/// restores SSE state, which is only true once the FPU is out of emulation.
unsafe fn enable_sse() {
    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::OSFXSR); // Enable FXSAVE/FXRSTOR
    cr4.insert(Cr4Flags::OSXMMEXCPT_ENABLE); // Enable SSE exceptions
    // SAFETY: both bits are additive and every other CR4 bit is carried over
    // from the read above. `OSFXSR` is the precondition of every `FXSAVE` and
    // `FXRSTOR` in this file, and it is set before any of them can run.
    unsafe { Cr4::write(cr4) };
}

/// Enable FSGSBASE so `rdgsbase`/`wrgsbase` can be used for fast per-CPU
/// data access (~1 cycle vs ~30 for rdmsr). Call once per CPU, before any
/// GS base read/write.
///
/// # Safety
/// Writes CR4 on the calling CPU, so the caller must not be migratable, and
/// must run before that CPU executes its first `rdgsbase`/`wrgsbase`. CPUID
/// must have reported the feature; every CPU QEMU and this kernel boot on does.
pub unsafe fn enable_fsgsbase() {
    let mut cr4 = Cr4::read();
    cr4.insert(Cr4Flags::FSGSBASE);
    // SAFETY: one additive bit, every other CR4 bit carried over from the read
    // above. It only unlocks two instructions; nothing observes it changing.
    unsafe { Cr4::write(cr4) };
}

/// Save FPU state using FXSAVE.
///
/// `XSAVEOPT` is the obvious thing to reach for here and it was measured:
/// save 32 -> 36 ns, restore 59 -> 83 ns, so it is a loss. The reason is that
/// it can only win by *skipping* components, and there are none to skip. The
/// state this kernel enables is x87 and SSE, exactly what `FXSAVE` writes, so
/// `XSAVE` covers the same registers and adds a 64-byte header plus
/// per-component work on top. Its init and modified optimisations do not
/// recover that: two threads handing off to each other `XRSTOR` from different
/// areas, which is what invalidates the modified tracking. `XSAVE` becomes the
/// right answer if `XCR0` ever grows a large optional component (AVX and
/// wider) that most threads leave alone; until then it is overhead.
///
/// # Safety
/// [`init_fpu`] must have run on this CPU, since `FXSAVE` is only available
/// once `CR0.EM` is clear and `CR4.OSFXSR` is set.
pub unsafe fn save_fpu_state(state: &mut FpuState) {
    // SAFETY: `FXSAVE` writes 512 bytes to a 16-byte aligned address.
    // `FpuState` is `#[repr(C, align(16))]` around exactly `[u8; 512]`, and the
    // `&mut` makes this the only reference to it for the duration.
    unsafe {
        core::arch::asm!(
            "fxsave [{}]",
            in(reg) state.data.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// Restore FPU state using FXRSTOR
///
/// # Safety
/// [`init_fpu`] must have run on this CPU, and `state` must be an image
/// [`save_fpu_state`] or [`init_fpu_state`] produced: `FXRSTOR` raises `#GP`
/// on a reserved bit set in the saved `MXCSR`, which a merely zeroed or
/// arbitrary 512 bytes can carry.
pub unsafe fn restore_fpu_state(state: &FpuState) {
    // SAFETY: `FXRSTOR` reads 512 bytes from a 16-byte aligned address, and
    // `FpuState` is `#[repr(C, align(16))]` around exactly `[u8; 512]`. The
    // image's provenance is the caller's obligation, above.
    unsafe {
        core::arch::asm!(
            "fxrstor [{}]",
            in(reg) state.data.as_ptr(),
            options(nostack, preserves_flags)
        );
    }
}

/// Give a thread the state it should start life with, and put the CPU in it.
///
/// Both halves matter. The area is what the thread will be restored from
/// later, and the *registers* have to be cleaned now because this runs on the
/// way in to a thread that has never had state of its own: `fninit` resets x87
/// and leaves SSE completely alone, so without the second half everything the
/// previously running thread left in `XMM0-15` is readable by this one, across
/// a process boundary.
///
/// # Safety
/// [`init_fpu`] must have run on this CPU. The FPU and SSE registers are
/// clobbered, so this may only run on the way in to a thread that has no state
/// of its own to lose.
pub unsafe fn init_fpu_state(state: &mut FpuState) {
    // SAFETY: `fninit` resets x87 with no operand. The save and restore
    // inherit this function's own contract, and the two writes in between stay
    // inside `state.data`'s 512 bytes -- `FXSAVE_XMM_END` is 416 and the
    // `MXCSR` field ends at 28.
    unsafe {
        core::arch::asm!("fninit", options(nostack, preserves_flags));
        save_fpu_state(state);
        state.data[FXSAVE_XMM_OFFSET..FXSAVE_XMM_END].fill(0);
        state.set_default_mxcsr();
        restore_fpu_state(state);
    }
}

/// `XMM0-15` inside an `FXSAVE` image, and the end of that region.
const FXSAVE_XMM_OFFSET: usize = 160;
const FXSAVE_XMM_END: usize = 416;
/// `MXCSR` inside an `FXSAVE` image, and its reset value: all exceptions
/// masked, round to nearest. Zero is a legal encoding meaning every SIMD
/// floating-point exception is *unmasked*, so a state area that is merely
/// zeroed raises `#XM` on the first inexact result the thread computes.
const FXSAVE_MXCSR_OFFSET: usize = 24;
const MXCSR_DEFAULT: u32 = 0x1F80;

// FPU/SSE state structure (512 bytes for FXSAVE)
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct FpuState {
    data: [u8; 512],
}

impl FpuState {
    /// Put the reset `MXCSR` into the image.
    fn set_default_mxcsr(&mut self) {
        self.data[FXSAVE_MXCSR_OFFSET..FXSAVE_MXCSR_OFFSET + 4]
            .copy_from_slice(&MXCSR_DEFAULT.to_le_bytes());
    }
}

impl Default for FpuState {
    fn default() -> Self {
        let mut state = Self { data: [0; 512] };
        state.set_default_mxcsr();
        state
    }
}
