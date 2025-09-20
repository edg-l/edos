pub mod init;

use core::time::Duration;

pub use init::init;
use x2apic::lapic::{LocalApic, LocalApicBuilder};

use crate::{interrupts::InterruptIndex, timer::get_timer_calibration};

// Get the lapic
pub fn get_lapic() -> LocalApic {
    LocalApicBuilder::new()
        .timer_vector(InterruptIndex::Timer as usize)
        .error_vector(InterruptIndex::Error as usize)
        .spurious_vector(InterruptIndex::Spurious as usize)
        .build()
        .unwrap()
}

pub fn set_apic_timer_and_enable(duration: Duration) {
    unsafe {
        let mut lapic = get_lapic();
        let timer = get_timer_calibration();
        lapic.set_timer_mode(x2apic::lapic::TimerMode::Periodic);
        lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1);
        lapic.set_timer_initial(timer.ticks_per_microsecond as u32 * duration.as_micros() as u32);
        lapic.enable_timer();
    }
}

pub fn disable_apic_timer() {
    unsafe {
        let mut lapic = get_lapic();
        lapic.disable_timer();
    }
}

fn duration_to_initial_count(duration: Duration, apic_frequency_hz: u64) -> u32 {
    let nanos = duration.as_nanos();
    let ticks = nanos.saturating_mul(apic_frequency_hz as u128) / 1_000_000_000u128;

    let clamped = ticks.clamp(1, u32::MAX as u128);
    clamped as u32
}
