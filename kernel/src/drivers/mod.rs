use crate::{graphics, thread::util::queue_spawn_kthread_named};

pub mod ahci;
pub mod dma;
pub mod fpu;
pub mod hpet;
pub mod keyboard;
pub mod msi;
pub mod pci;
pub mod rtc;
pub mod tty;

pub fn init_drivers() {
    hpet::driver::init();
    unsafe { fpu::init_fpu() };
    pci::init(); // pci init is blocking
    ahci::init(); // must be after pci
    graphics::init();
    tty::init();
    queue_spawn_kthread_named("keyboard", keyboard::driver_main as u64);
}
