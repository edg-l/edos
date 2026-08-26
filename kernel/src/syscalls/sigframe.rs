//! Running a userspace signal handler, and coming back from it.
//!
//! Delivery happens at the syscall-return boundary and nowhere else. That is
//! the one place the whole of a thread's user context is already sitting in a
//! `SyscallContext` the dispatcher owns, so redirecting it to a handler is a
//! matter of rewriting that structure rather than manufacturing a new one; it
//! is also a place where the thread provably holds no kernel guard, which is
//! what makes running arbitrary user code from here safe.
//!
//! The consequence worth knowing: a thread that never makes a syscall never
//! runs a handler. Default actions still reach it, because a timer tick out of
//! ring 3 applies those, so Ctrl+C still kills a spinning process — it just
//! cannot be *caught* by one.

use core::sync::atomic::Ordering;

use crate::{
    memory::vma::USER_VA_END,
    syscalls::{Errno, SyscallContext},
    thread::{scheduler::current_thread, signal, thread::Thread},
    util::uaccess::{try_copy_from_user, try_copy_to_user},
};

/// Identifies a frame this kernel built, so a `sigreturn` with a corrupted or
/// invented stack is refused rather than loading a context from whatever the
/// address happened to hold.
const SIGFRAME_MAGIC: u64 = 0x5349_4746_524d_3031; // "SIGFRM01"

/// What a handler has to be given back when it returns.
///
/// `saved` is the entire interrupted context, including the syscall's return
/// value in `rax`, so a handler that runs between a call finishing and
/// userspace seeing its result is invisible to the interrupted code.
#[repr(C)]
#[derive(Clone, Copy)]
struct SigFrame {
    magic: u64,
    signum: u64,
    saved_blocked: u64,
    saved: SyscallContext,
}

/// The System V red zone, which a signal frame must not land in: the
/// interrupted function may still be using it.
const RED_ZONE: u64 = 128;

/// Deliver one pending handled signal by redirecting `ctx` at its handler.
///
/// One per return rather than all of them: the rest stay pending and arrive at
/// the next boundary, which keeps the stack usage of a signal storm bounded by
/// the number of returns rather than by the number of signals.
pub fn deliver_pending_handler(ctx: &mut SyscallContext) {
    let Some(thread) = current_thread() else {
        return;
    };

    let deliverable = thread.signal.pending.load(Ordering::Acquire) & !thread.signal.blocked();
    if deliverable == 0 {
        return;
    }

    for signum in 1..32u32 {
        if deliverable & (1 << signum) == 0 {
            continue;
        }
        let handler = thread.signal.get_handler(signum);
        if handler <= signal::SIG_IGN {
            continue;
        }
        // Consumed only once the frame is known to be writable, so a signal is
        // never lost to a stack that could not take it.
        if build_frame(&thread, ctx, signum, handler) {
            thread.signal.clear(signum);
        }
        return;
    }
}

/// Push a `SigFrame` onto the user stack and point `ctx` at the handler.
///
/// Returns whether the frame was written. A failure here means the user stack
/// is unusable, which is not something a handler could recover from anyway, so
/// the thread is killed rather than being sent to a handler with a broken
/// stack.
fn build_frame(thread: &Thread, ctx: &mut SyscallContext, signum: u32, handler: u64) -> bool {
    let restorer = thread.signal.restorer.load(Ordering::Acquire);
    if restorer == 0 || restorer >= USER_VA_END || handler >= USER_VA_END {
        return false;
    }

    let frame = SigFrame {
        magic: SIGFRAME_MAGIC,
        signum: signum as u64,
        saved_blocked: thread.signal.blocked() as u64,
        saved: *ctx,
    };

    // Below the red zone, then aligned down: the ABI wants rsp+8 to be
    // 16-aligned at function entry, and the return address pushed below the
    // frame is that +8.
    let frame_base = ctx
        .rsp
        .saturating_sub(RED_ZONE)
        .saturating_sub(size_of::<SigFrame>() as u64)
        & !0xf;
    let entry_rsp = frame_base.saturating_sub(8);

    if frame_base == 0 || entry_rsp >= USER_VA_END {
        return false;
    }

    let wrote_frame = unsafe {
        try_copy_to_user(
            frame_base as *mut u8,
            &frame as *const SigFrame as *const u8,
            size_of::<SigFrame>(),
        )
    };
    let wrote_return = unsafe {
        try_copy_to_user(
            entry_rsp as *mut u8,
            &restorer as *const u64 as *const u8,
            8,
        )
    };
    if !wrote_frame || !wrote_return {
        return false;
    }

    thread.signal.block_during_handler(signum);

    ctx.rsp = entry_rsp;
    ctx.rip = handler;
    ctx.rdi = signum as u64;
    true
}

/// Return from a signal handler: reload the context the frame saved.
///
/// The value answered is `ctx.rax` as the frame held it, so the interrupted
/// syscall's own result survives the detour rather than being overwritten by
/// this call's.
pub fn sys_sigreturn(ctx: &mut SyscallContext) -> Result<u64, Errno> {
    let thread = current_thread().ok_or(Errno::EINVAL)?;

    let mut frame = SigFrame {
        magic: 0,
        signum: 0,
        saved_blocked: 0,
        saved: *ctx,
    };
    let read = ctx.rsp < USER_VA_END
        && unsafe {
            try_copy_from_user(
                &mut frame as *mut SigFrame as *mut u8,
                ctx.rsp as *const u8,
                size_of::<SigFrame>(),
            )
        };

    // A frame this kernel did not write is a forged one. Refusing it matters
    // because the restore below loads rip, rsp and rflags wholesale.
    if !read || frame.magic != SIGFRAME_MAGIC {
        return Err(Errno::EINVAL);
    }

    thread.signal.restore_blocked(frame.saved_blocked as u32);

    // rflags comes from the frame with the interrupt flag forced on and the
    // privileged bits masked off: the saved value passed through user hands on
    // its way here and must not be able to re-enter the kernel's own flags.
    let mut restored = frame.saved;
    restored.rflags = (restored.rflags & USER_RFLAGS_MASK) | RFLAGS_INTERRUPT;
    if restored.rip >= USER_VA_END || restored.rsp >= USER_VA_END {
        return Err(Errno::EINVAL);
    }

    *ctx = restored;
    Ok(ctx.rax)
}

/// Flags userspace is allowed to set. Everything else — IOPL, NT, the
/// virtualisation bits — is cleared, so a forged frame cannot grant itself
/// privileges the process did not have.
const USER_RFLAGS_MASK: u64 = 0x0000_08d5; // CF PF AF ZF SF TF DF plus reserved bit 1
const RFLAGS_INTERRUPT: u64 = 1 << 9;
