pub mod init;

pub use init::init;
use x2apic::lapic::LocalApic;

use crate::{util::per_cpu::get_percpu_data};


pub fn current_apic_id() -> u32 {
    unsafe { (*get_percpu_data().lapic).id() }
}

// Get the lapic
pub fn get_lapic() -> &'static mut LocalApic {
    unsafe {
        get_percpu_data().lapic.as_mut().unwrap_unchecked()
    }
}
