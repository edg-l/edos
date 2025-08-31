use x86_64::instructions::port::Port;

use crate::{apic::get_lapic, serial_println};

// TODO: Fallback to pit if HPET not found.

#[allow(unused)]
pub struct TimerCalibration {
    pub apic_frequency_hz: u64,
    pub ticks_per_microsecond: u64,
    pub tsc_frequency_hz: u64,
}

impl TimerCalibration {
    /// Calibrate APIC timer frequency using PIT as reference
    pub fn calibrate_apic_timer() -> TimerCalibration {
        const PIT_FREQUENCY: u64 = 1193182; // Hz
        const CALIBRATION_MS: u64 = 35; // Calibrate for 35ms
        const PIT_TICKS_FOR_CALIBRATION: u16 = (PIT_FREQUENCY * CALIBRATION_MS / 1000) as u16;

        serial_println!("Calibrating APIC timer frequency...");

        // 1. Setup PIT for one-shot mode
        unsafe {
            Self::setup_pit_oneshot(PIT_TICKS_FOR_CALIBRATION);
        }

        // 2. Start APIC timer with maximum count
        let apic_start_count = 0xFFFFFFFF;
        let tsc_start = unsafe { core::arch::x86_64::_rdtsc() };
        unsafe {
            let lapic = get_lapic();
            lapic.set_timer_mode(x2apic::lapic::TimerMode::OneShot);
            lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1); // No division for calibration
            lapic.set_timer_initial(apic_start_count);
        }

        // 3. Wait for PIT to finish (busy wait is fine for calibration)
        Self::wait_for_pit_completion();

        // 4. Read APIC timer current count
        let apic_end_count = unsafe { get_lapic().timer_current() };
        let tsc_end = unsafe { core::arch::x86_64::_rdtsc() };

        // 5. Calculate APIC frequency
        let apic_ticks_elapsed = apic_start_count - apic_end_count;
        let tsc_ticks_elapsed = tsc_end - tsc_start;
        let time_elapsed_us = CALIBRATION_MS * 1000; // microseconds

        // APIC frequency = ticks_elapsed / time_elapsed_seconds
        let apic_frequency_hz = (apic_ticks_elapsed as u64 * 1_000) / (time_elapsed_us / 1000);
        let tsc_frequency_hz = (tsc_ticks_elapsed * 1_000) / (time_elapsed_us / 1000);
        let ticks_per_microsecond = apic_ticks_elapsed as u64 / time_elapsed_us;

        serial_println!("APIC Timer Calibration Results:");
        serial_println!("  Calibration time: {}ms", CALIBRATION_MS);
        serial_println!("  APIC ticks elapsed: {}", apic_ticks_elapsed);
        serial_println!(
            "  APIC frequency: {} Hz ({}) MHz",
            apic_frequency_hz,
            apic_frequency_hz / 1_000_000
        );
        serial_println!(
            "  TSC frequency: {} Hz ({:.2}) GHz",
            tsc_frequency_hz,
            tsc_frequency_hz as f64 / 1_000_000_000.0
        );
        serial_println!("  Ticks per microsecond: {}", ticks_per_microsecond);

        TimerCalibration {
            apic_frequency_hz,
            ticks_per_microsecond,
            tsc_frequency_hz,
        }
    }

    #[inline(always)]
    unsafe fn setup_pit_oneshot(ticks: u16) {
        unsafe {
            // PIT Command: Channel 0, Lo/Hi byte, Mode 0 (interrupt on terminal count), Binary
            Port::new(0x43).write(0b00110000u8); // Command register

            // Set count (LSB then MSB)
            Port::new(0x40).write((ticks & 0xFF) as u8); // LSB
            Port::new(0x40).write(((ticks >> 8) & 0xFF) as u8); // MSB
        }
    }

    #[inline(always)]
    fn wait_for_pit_completion() {
        // Poll PIT status until count reaches zero
        loop {
            unsafe {
                // Read-back command: Channel 0, latch count
                Port::new(0x43).write(0b11100010u8);

                // Read status
                let status: u8 = Port::new(0x40).read();

                // Bit 7 = output pin state (goes high when count reaches 0)
                if status & 0x80 != 0 {
                    break;
                }
            }

            core::hint::spin_loop();
        }
    }
}

// Global calibration result
static TIMER_CALIBRATION: spin::Once<TimerCalibration> = spin::Once::new();

pub fn get_timer_calibration() -> &'static TimerCalibration {
    TIMER_CALIBRATION.call_once(TimerCalibration::calibrate_apic_timer)
}

pub use crate::drivers::hpet::instant::HpetInstant as Instant;

/// Boot time reference point
static BOOT_TIME: spin::Once<Instant> = spin::Once::new();

/// Initialize the boot time reference
pub fn init_boot_time() {
    BOOT_TIME.call_once(Instant::now);
}

/// Get time elapsed since boot in microseconds
pub fn uptime_us() -> u64 {
    if let Some(boot_instant) = BOOT_TIME.get() {
        boot_instant.elapsed().as_micros() as u64
    } else {
        0
    }
}
