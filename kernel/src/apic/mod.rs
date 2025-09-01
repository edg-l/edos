pub mod init;

pub use init::init;
use x2apic::lapic::{LocalApic, LocalApicBuilder};

use crate::interrupts::InterruptIndex;

// Get the lapic
pub fn get_lapic() -> LocalApic {
    LocalApicBuilder::new()
        .timer_vector(InterruptIndex::Timer as usize)
        .error_vector(InterruptIndex::Error as usize)
        .spurious_vector(InterruptIndex::Spurious as usize)
        .build()
        .unwrap()
}
