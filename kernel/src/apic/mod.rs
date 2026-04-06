pub mod init;

use core::time::Duration;

pub use init::init;
use x2apic::lapic::LocalApic;

use crate::{timer::get_timer_calibration, util::per_cpu::get_percpu_data};

// Get the lapic
pub fn get_lapic() -> &'static mut LocalApic {
    // TODO: maybe put behind a loc
    unsafe { get_percpu_data().lapic.get().as_mut().unwrap() }
}

pub fn set_apic_timer_and_enable(duration: Duration) {
    unsafe {
        let lapic = get_lapic();
        let timer = get_timer_calibration();
        lapic.set_timer_mode(x2apic::lapic::TimerMode::OneShot);
        lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1);
        let micros = duration.as_micros() as u64;
        let count = (timer.ticks_per_microsecond.saturating_mul(micros)) as u32;
        lapic.set_timer_initial(if count == 0 { 1 } else { count });
        lapic.enable_timer();
    }
}

pub fn set_apic_timer(duration: Duration) {
    let lapic = get_lapic();
    let timer = get_timer_calibration();
    // Compute initial count, clamping to at least 1. Writing 0 to the
    // initial count register stops the one-shot APIC timer permanently,
    // so sub-microsecond durations (which truncate to 0) must be avoided.
    let micros = duration.as_micros() as u64;
    let count = (timer.ticks_per_microsecond.saturating_mul(micros)) as u32;
    unsafe {
        lapic.set_timer_initial(if count == 0 { 1 } else { count });
    }
}
