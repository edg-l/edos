pub mod init;

pub use init::init;
use x2apic::lapic::LocalApic;

use crate::{apic::init::enable_apic, memory::APIC_BASE, util::core_local::CoreLocal};

/// The current core local apic.
pub static LAPIC: CoreLocal<LocalApic> = CoreLocal::new(|| unsafe { enable_apic() });

pub fn current_apic_id() -> u32 {
    let apic_id_reg = APIC_BASE.as_u64() + 0x20;
    unsafe { core::ptr::read_volatile(apic_id_reg as *const u32) >> 24 }
}
