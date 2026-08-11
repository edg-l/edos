use x86_64::VirtAddr;

#[derive(Debug)]
pub struct HpetTimer {
    /// Femtoseconds per tick, from the capability register.
    pub frequency: u64,
    pub base: VirtAddr,
}

impl HpetTimer {
    pub fn ticks_to_nanos(&self, ticks: u64) -> u64 {
        // Convert femtoseconds to nanoseconds: divide by 1,000,000
        // Use u128 to avoid overflow for large tick deltas
        ((ticks as u128 * self.frequency as u128) / 1_000_000) as u64
    }
}
