pub mod init;

use core::time::Duration;

pub use init::init;
use x2apic::lapic::LocalApic;

use crate::{timer::get_timer_calibration, util::per_cpu::get_percpu_data};

/// The calling CPU's own LAPIC.
///
/// The register block is per-CPU, so no other CPU can reach the one this
/// returns and a lock would serialise nothing. What the `&'static mut` does
/// not carry is the per-CPU rule: the reference names the LAPIC of the CPU the
/// GS base was read on, so it stops being the caller's own the moment the
/// thread can migrate. Use it inside the section that read it.
pub fn get_lapic() -> &'static mut LocalApic {
    // SAFETY: `lapic` holds a `Box::leak`ed `LocalApic` that `enable_lapic`
    // stored on this CPU's own block before any other code on it runs, so the
    // pointer is non-null, aligned and lives for the rest of the boot. It is
    // reachable only through this CPU's GS base, so the `&'static mut` is
    // unique for as long as the caller stays on the CPU whose base was read —
    // which is the migration rule stated above, not an aliasing one.
    unsafe { get_percpu_data().lapic.get().as_mut().unwrap() }
}

/// The shortest interval the one-shot timer is ever armed for.
///
/// A deadline that has effectively arrived asks for a timer of nearly zero
/// length, and the two ways to answer that are both wrong: writing 0 to the
/// initial count stops the timer permanently, and a count of a handful of
/// ticks fires again before the handler that armed it has returned, so the CPU
/// services interrupts instead of the work it was about to do. Neither showed
/// while durations came from a counter that cost microseconds to read.
const MIN_TIMER_INTERVAL: Duration = Duration::from_micros(10);

/// Initial count for a one-shot of `duration`, floored at
/// [`MIN_TIMER_INTERVAL`] and saturated rather than truncated at the top.
fn timer_count(duration: Duration) -> u32 {
    let micros = duration.max(MIN_TIMER_INTERVAL).as_micros() as u64;
    get_timer_calibration()
        .ticks_per_microsecond
        .saturating_mul(micros)
        .clamp(1, u32::MAX as u64) as u32
}

pub fn set_apic_timer_and_enable(duration: Duration) {
    let lapic = get_lapic();
    // SAFETY: these four write the calling CPU's own LVT timer entry, divide
    // register and initial count, which no other CPU can reach. The vector was
    // fixed by `LocalApicBuilder` at `enable_lapic` time and is not touched
    // here, so arming the timer can only deliver `InterruptIndex::Timer`.
    unsafe {
        lapic.set_timer_mode(x2apic::lapic::TimerMode::OneShot);
        lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1);
        lapic.set_timer_initial(timer_count(duration));
        lapic.enable_timer();
    }
}

/// Arm the one-shot for `duration`, and report the interval actually
/// programmed, which is `duration` raised to [`MIN_TIMER_INTERVAL`].
///
/// Callers that remember when the timer will fire must remember what comes
/// back rather than what they asked for, or they will believe a floored timer
/// fires earlier than it does.
pub fn set_apic_timer(duration: Duration) -> Duration {
    let armed = duration.max(MIN_TIMER_INTERVAL);
    let lapic = get_lapic();
    // SAFETY: a write to the calling CPU's own initial-count register, as in
    // `set_apic_timer_and_enable`. The mode, divide and vector were programmed
    // by that function and re-arming does not disturb them.
    unsafe {
        lapic.set_timer_initial(timer_count(armed));
    }
    armed
}
