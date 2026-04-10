use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::VirtAddr;

use crate::{
    apic::get_lapic,
    interrupts::InterruptIndex,
    smp::{current_cpu_index, lapic_id_for_cpu, online_cpu_mask},
};

use x86_64::structures::idt::InterruptStackFrame;

/// A single global TLB shootdown request slot. Only one shootdown is in flight
/// at a time (serialized by `active`). No heap allocation.
pub struct TlbFlushRequest {
    pub start_addr: AtomicU64,
    pub page_count: AtomicU64,
    /// Bitmask of CPU indices that have not yet acknowledged the flush.
    pub pending_mask: AtomicU64,
    /// Serialization lock: true while a shootdown is in progress.
    pub active: AtomicBool,
}

// SAFETY: All fields are atomics; the struct has no interior mutability beyond them.
unsafe impl Sync for TlbFlushRequest {}

pub static FLUSH_REQUEST: TlbFlushRequest = TlbFlushRequest {
    start_addr: AtomicU64::new(0),
    page_count: AtomicU64::new(0),
    pending_mask: AtomicU64::new(0),
    active: AtomicBool::new(false),
};

/// Send a TLB shootdown IPI to all other online CPUs, flush locally, and wait
/// for all remote CPUs to acknowledge before returning.
///
/// `page_count == u64::MAX` means flush the entire TLB (full reload).
///
/// This function must be called with interrupts **enabled** so that the caller
/// CPU can receive reschedule or other IPIs while spinning, and so that target
/// CPUs can receive the shootdown IPI.
pub fn tlb_shootdown(start: VirtAddr, page_count: u64) {
    debug_assert!(
        x86_64::instructions::interrupts::are_enabled(),
        "tlb_shootdown called with interrupts disabled"
    );

    // Build the bitmask of target CPUs (all online CPUs except self).
    let my_idx = current_cpu_index();
    let target_mask = online_cpu_mask() & !(1u64 << my_idx);

    // Fast path: no other CPUs to notify, just flush locally.
    if target_mask == 0 {
        do_local_flush(start, page_count);
        return;
    }

    // Acquire the serialization lock (spin until no other shootdown is in flight).
    while FLUSH_REQUEST
        .active
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }

    // Publish the range.
    FLUSH_REQUEST
        .start_addr
        .store(start.as_u64(), Ordering::Release);
    FLUSH_REQUEST
        .page_count
        .store(page_count, Ordering::Release);

    // Publish the pending mask before sending any IPI so that the handlers
    // can safely clear their bits.
    FLUSH_REQUEST
        .pending_mask
        .store(target_mask, Ordering::Release);

    // Send IPI to each target CPU. x2APIC send_ipi is an MSR write and
    // does not require interrupts to be disabled.
    let mut mask = target_mask;
    while mask != 0 {
        let idx = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        let lapic_id = lapic_id_for_cpu(idx);
        unsafe { get_lapic().send_ipi(InterruptIndex::TlbShootdown as u8, lapic_id) };
    }

    // Flush the local TLB now.
    do_local_flush(start, page_count);

    // Wait for all remote CPUs to acknowledge, with a timeout.
    const TIMEOUT_ITERS: u64 = 10_000_000;
    let mut iters = 0u64;
    while FLUSH_REQUEST.pending_mask.load(Ordering::Acquire) != 0 {
        core::hint::spin_loop();
        iters += 1;
        if iters >= TIMEOUT_ITERS {
            let remaining = FLUSH_REQUEST.pending_mask.load(Ordering::Acquire);
            if remaining != 0 {
                crate::log!(
                    "tlb_shootdown: timeout waiting for CPUs (mask={:#x}), forcing clear",
                    remaining
                );
                // Force clear to avoid hanging. The lagging CPUs will flush
                // redundantly when they eventually process the IPI, which is safe.
                FLUSH_REQUEST.pending_mask.store(0, Ordering::Release);
            }
            break;
        }
    }

    // Release the serialization lock.
    FLUSH_REQUEST.active.store(false, Ordering::Release);
}

/// Returns true if TLB shootdown is necessary (more than one CPU online).
pub fn shootdown_needed() -> bool {
    crate::smp::cpu_count() > 1
}

/// Flush the entire TLB (including global pages) on all CPUs.
pub fn tlb_shootdown_all() {
    tlb_shootdown(VirtAddr::new(0), u64::MAX);
}

/// Perform a local TLB flush for the given range.
fn do_local_flush(start: VirtAddr, page_count: u64) {
    // Full reload: use the CR4 PGE toggle trick (also flushes global pages).
    if page_count == u64::MAX || page_count > 32 {
        crate::smp::tlb_flush_all_including_global();
        return;
    }

    // Targeted: invlpg for each page in the range.
    let base = start.as_u64();
    for i in 0..page_count {
        let addr = base + i * 4096;
        unsafe {
            core::arch::asm!("invlpg [{0}]", in(reg) addr, options(nostack, preserves_flags));
        }
    }
}

/// x86 interrupt handler for TLB shootdown IPIs.
///
/// Reads the flush range from FLUSH_REQUEST, performs the local flush, clears
/// this CPU's bit in the pending mask, and sends EOI.
///
/// This function must be allocation-free.
pub extern "x86-interrupt" fn tlb_shootdown_handler(_stack_frame: InterruptStackFrame) {
    let start_raw = FLUSH_REQUEST.start_addr.load(Ordering::Acquire);
    let page_count = FLUSH_REQUEST.page_count.load(Ordering::Acquire);

    do_local_flush(VirtAddr::new_truncate(start_raw), page_count);

    let my_idx = current_cpu_index();
    FLUSH_REQUEST
        .pending_mask
        .fetch_and(!(1u64 << my_idx), Ordering::Release);

    unsafe { get_lapic().end_of_interrupt() };
}
