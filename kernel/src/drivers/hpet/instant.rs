use core::{ops::Add, time::Duration};

use x86_64::VirtAddr;

use crate::drivers::hpet::driver::get_hpet_timer;

#[derive(Debug)]
pub struct HpetTimer {
    pub frequency: u64,
    pub base: VirtAddr,
}

impl HpetTimer {
    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        // Convert femtoseconds to nanoseconds: divide by 1,000,000
        (ticks * self.frequency) / 1_000_000
    }

    pub fn nanos_to_ticks(&self, nanos: u128) -> u64 {
        ((nanos * 1_000_000) / self.frequency as u128) as u64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct HpetInstant {
    counter_value: u64,
}

impl HpetInstant {
    pub fn now() -> Self {
        Self {
            counter_value: get_hpet_timer().expect("hpet not present").get_counter(),
        }
    }

    pub const fn tick(&self) -> u64 {
        self.counter_value
    }

    pub fn elapsed(&self) -> Duration {
        let now = Self::now();
        let ticks = now.counter_value.saturating_sub(self.counter_value);
        let nanos = get_hpet_timer().unwrap().ticks_to_nanos(ticks);
        Duration::from_nanos(nanos)
    }

    pub fn duration_since(&self, earlier: HpetInstant) -> Duration {
        let ticks = self.counter_value.saturating_sub(earlier.counter_value);
        let nanos = get_hpet_timer().unwrap().ticks_to_nanos(ticks);
        Duration::from_nanos(nanos)
    }

    pub const fn from_tick(tick: u64) -> Self {
        Self {
            counter_value: tick,
        }
    }
}

impl Add<Duration> for HpetInstant {
    type Output = Self;
    fn add(self, rhs: Duration) -> Self::Output {
        let ticks = get_hpet_timer().unwrap().nanos_to_ticks(rhs.as_nanos());
        Self {
            counter_value: self.counter_value.wrapping_add(ticks),
        }
    }
}
