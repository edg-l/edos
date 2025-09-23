pub mod init;

use core::time::Duration;

pub use init::init;
use x2apic::lapic::LocalApic;

use crate::{timer::get_timer_calibration, util::per_cpu::get_percpu_data};

// Get the lapic
pub fn get_lapic() -> &'static mut LocalApic {
    // TODO: maybe put behind a loc
    unsafe { get_percpu_data().lapic.as_mut().unwrap() }
}

pub fn set_apic_timer_and_enable(duration: Duration) {
    unsafe {
        let lapic = get_lapic();
        let timer = get_timer_calibration();
        lapic.set_timer_mode(x2apic::lapic::TimerMode::OneShot);
        lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1);
        lapic.set_timer_initial(timer.ticks_per_microsecond as u32 * duration.as_micros() as u32);
        lapic.enable_timer();
    }
}

// Deadline is a instant
pub fn set_apic_timer(duration: Duration) {
    let lapic = get_lapic();
    let timer = get_timer_calibration();
    unsafe {
        lapic.set_timer_initial(timer.ticks_per_microsecond as u32 * duration.as_micros() as u32);
    }
}
