use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use x86_64::VirtAddr;

use crate::{
    apic::get_lapic,
    interrupts::InterruptIndex,
    smp::{current_cpu_index, lapic_id_for_cpu, online_cpu_mask},
    thread::preempt::preempt_disable,
};

use x86_64::structures::idt::InterruptStackFrame;

/// A single global TLB shootdown request slot. Only one shootdown is in flight
/// at a time (serialized by `active`). No heap allocation.
pub struct TlbFlushRequest {
    pub start_addr: AtomicU64,
    pub page_count: AtomicU64,
    /// Bitmask of CPU indices that have not yet acknowledged the flush.
    pub pending_mask: AtomicU64,
    /// Bumped once per round. A handler that reads the range for one round and
    /// only gets to acknowledge after that round ended must not acknowledge the
    /// round now in flight, which it has not flushed for.
    pub generation: AtomicU64,
    /// Serialization lock: true while a shootdown is in progress.
    pub active: AtomicBool,
}

// SAFETY: All fields are atomics; the struct has no interior mutability beyond them.
unsafe impl Sync for TlbFlushRequest {}

pub static FLUSH_REQUEST: TlbFlushRequest = TlbFlushRequest {
    start_addr: AtomicU64::new(0),
    page_count: AtomicU64::new(0),
    pending_mask: AtomicU64::new(0),
    generation: AtomicU64::new(0),
    active: AtomicBool::new(false),
};

/// Spins per acknowledgement attempt before the IPI is re-sent. A target with
/// interrupts disabled is the normal reason to wait: the longest such region is
/// a serial write, hundreds of microseconds, which this is comfortably beyond.
const ACK_SPIN_LIMIT: u64 = 10_000_000;
/// Re-sends before the laggards are asked whether they are running at all.
/// Covers an IPI that was never delivered; past this the question stops being
/// "was it delivered" and becomes "is anyone there".
const ACK_ATTEMPTS: u32 = 3;

/// Send a TLB shootdown IPI to all other online CPUs, flush locally, and wait
/// for all remote CPUs to acknowledge before returning.
///
/// `page_count == u64::MAX` means flush the entire TLB (full reload).
///
/// This function must be called with interrupts **enabled** so that the caller
/// CPU can receive reschedule or other IPIs while spinning, and so that target
/// CPUs can receive the shootdown IPI.
///
/// Call it whenever a mapping is torn down, including on a machine with one
/// CPU online. The local flush below is not an optimisation of the IPI round —
/// it is the *only* invalidation an unmapper gets when it dropped the per-page
/// `MapperFlush` with `ignore()`, which every caller that batches a range
/// does. Guarding the call on "is anyone else running" leaves that CPU reading
/// through a translation to a frame the allocator has already handed out again.
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
    // Preemption stays off for the round: every other CPU wanting a shootdown
    // spins on this flag, so a descheduled holder stalls all of them.
    let _no_preempt = preempt_disable();
    while FLUSH_REQUEST
        .active
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        core::hint::spin_loop();
    }

    // Open a new round before publishing anything it describes, so a handler
    // still finishing the previous one cannot acknowledge this one.
    FLUSH_REQUEST.generation.fetch_add(1, Ordering::AcqRel);

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

    send_shootdown_ipis(target_mask);

    // Flush the local TLB now.
    do_local_flush(start, page_count);

    // Wait for every remote CPU to acknowledge. There is no safe way to give
    // up: returning means telling the caller that no CPU holds the old
    // translation any more, and the caller is entitled to free or reuse the
    // page on the strength of it. A CPU that never acknowledged is still
    // reading through a stale entry, so abandoning the wait trades a stall for
    // silent memory corruption. Re-send, and then ask whether the CPU that owes
    // an answer is executing at all — a stall is survivable and only the CPU
    // that is running and ignoring the vector is this kernel's fault.
    let mut round = 0u32;
    'wait: loop {
        for _ in 0..ACK_SPIN_LIMIT {
            if FLUSH_REQUEST.pending_mask.load(Ordering::Acquire) == 0 {
                break 'wait;
            }
            core::hint::spin_loop();
        }
        let remaining = FLUSH_REQUEST.pending_mask.load(Ordering::Acquire);
        if remaining == 0 {
            break;
        }
        round += 1;

        // Past the re-sends, a CPU that has not answered is either not
        // executing — descheduled by a hypervisor, or wedged with interrupts
        // off — or executing and not taking this interrupt. Only the second is
        // a fault here, and waiting is the only correct response to the first:
        // the frame cannot be reused while anyone still holds a translation to
        // it, and a CPU that resumes later resumes with its TLB as it left it.
        if round >= ACK_ATTEMPTS {
            let alive = laggards_taking_interrupts(remaining);
            assert!(
                alive == 0,
                "tlb_shootdown: CPUs {alive:#x} are taking timer interrupts and not this \
                 shootdown of {page_count} page(s) at {start:?}; the IPI is being delivered \
                 to a CPU that will not act on it"
            );
            if round == ACK_ATTEMPTS {
                crate::log!(
                    "tlb_shootdown: CPUs {remaining:#x} are not executing; waiting rather \
                     than freeing a page they may still translate"
                );
            }
        }

        crate::log!("tlb_shootdown: re-sending IPI to CPUs {remaining:#x}");
        send_shootdown_ipis(remaining);
    }

    // Release the serialization lock.
    FLUSH_REQUEST.active.store(false, Ordering::Release);
}

/// Which of `mask` are taking timer interrupts while owing an acknowledgement.
///
/// Sampled twice around a wait long enough to cover several ticks, because a
/// heartbeat is only evidence when it is seen to advance. A CPU that ticks
/// while ignoring the shootdown vector is a fault in this kernel; one whose
/// heartbeat is frozen is not running at all, and nothing here can hurry it.
fn laggards_taking_interrupts(mask: u64) -> u64 {
    let mut before = [0u64; 64];
    for idx in bits(mask) {
        before[idx] = crate::smp::cpu_heartbeat(idx);
    }

    spin_for_ms(LIVENESS_SAMPLE_MS);

    let mut alive = 0u64;
    for idx in bits(mask) {
        if crate::smp::cpu_heartbeat(idx) != before[idx] {
            alive |= 1 << idx;
        }
    }
    alive
}

/// The set bits of `mask`, as indices.
fn bits(mask: u64) -> impl Iterator<Item = usize> {
    (0..64).filter(move |idx| mask & (1 << idx) != 0)
}

/// Long enough for an idle CPU to have ticked several times: `run_idle` arms
/// its timer at most 100 ms out before halting, so a CPU that is executing at
/// all advances its heartbeat well inside this.
const LIVENESS_SAMPLE_MS: u64 = 300;

/// Spin for a wall-clock interval.
///
/// The HPET rather than a spin count, because this is a claim about elapsed
/// time and a count is a claim about instructions retired — which is the thing
/// a hypervisor descheduling the CPU changes. Falls back to a count where there
/// is no HPET, since a rough wait beats no answer.
fn spin_for_ms(ms: u64) {
    let Some(hpet) = crate::drivers::hpet::driver::get_hpet_timer() else {
        for _ in 0..ACK_SPIN_LIMIT {
            core::hint::spin_loop();
        }
        return;
    };
    let start = hpet.get_counter();
    let target_ns = ms * 1_000_000;
    while hpet.ticks_to_nanos(hpet.get_counter().wrapping_sub(start)) < target_ns {
        core::hint::spin_loop();
    }
}

/// Send the shootdown IPI to every CPU in `mask`. x2APIC `send_ipi` is an MSR
/// write and does not require interrupts to be disabled.
fn send_shootdown_ipis(mask: u64) {
    let mut mask = mask;
    while mask != 0 {
        let idx = mask.trailing_zeros() as usize;
        mask &= mask - 1;
        let lapic_id = lapic_id_for_cpu(idx);
        unsafe { get_lapic().send_ipi(InterruptIndex::TlbShootdown as u8, lapic_id) };
    }
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
    let generation = FLUSH_REQUEST.generation.load(Ordering::Acquire);
    let start_raw = FLUSH_REQUEST.start_addr.load(Ordering::Acquire);
    let page_count = FLUSH_REQUEST.page_count.load(Ordering::Acquire);

    do_local_flush(VirtAddr::new_truncate(start_raw), page_count);

    // Acknowledge the round whose range was just flushed, never a later one.
    // A re-sent IPI can be delivered twice, so the second delivery may land
    // after its round closed; crediting the round in flight would report a
    // flush this CPU never performed. Skipping is safe because a round that is
    // still waiting on this CPU has an IPI of its own latched for it.
    if FLUSH_REQUEST.generation.load(Ordering::Acquire) == generation {
        let my_idx = current_cpu_index();
        FLUSH_REQUEST
            .pending_mask
            .fetch_and(!(1u64 << my_idx), Ordering::Release);
    }

    unsafe { get_lapic().end_of_interrupt() };
}
