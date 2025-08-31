#![expect(unused)]

use core::time::Duration;

use x86_64::VirtAddr;

use crate::drivers::hpet::driver::get_hpet_timer;

pub struct HpetTimer {
    pub frequency: u64,
    pub base: VirtAddr,
}

pub struct HpetInstant {
    counter_value: u64,
}

impl HpetInstant {
    pub fn now() -> Self {
        Self {
            counter_value: get_hpet_timer().expect("hpet not present").get_counter(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        let now = Self::now();
        let ticks = now.counter_value.saturating_sub(self.counter_value);
        let nanos = self.ticks_to_nanos(ticks);
        Duration::from_nanos(nanos)
    }

    pub fn duration_since(&self, earlier: HpetInstant, timer: &HpetTimer) -> Duration {
        let ticks = self.counter_value.saturating_sub(earlier.counter_value);
        let nanos = self.ticks_to_nanos(ticks);
        Duration::from_nanos(nanos)
    }

    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        // Convert femtoseconds to nanoseconds: divide by 1,000,000
        let timer = get_hpet_timer().unwrap();
        (ticks * timer.frequency) / 1_000_000
    }
}
