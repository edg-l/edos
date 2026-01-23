use crate::{graphics, thread::util::queue_spawn_kthread_named};

pub mod ahci;
pub mod dma;
pub mod fpu;
pub mod hpet;
pub mod keyboard;
pub mod mouse;
pub mod msi;
pub mod pci;
pub mod random;
pub mod rtc;
pub mod tty;
pub mod vga;

pub fn init_drivers() {
    hpet::driver::init();
    unsafe { fpu::init_fpu() };
    pci::init(); // pci init is blocking
    ahci::init(); // must be after pci
    vga::init();
    graphics::init();
    tty::init();
    random::init();
    queue_spawn_kthread_named("keyboard", keyboard::driver_main as *const () as u64);
    mouse::init();
    queue_spawn_kthread_named("mouse", mouse::driver_main as *const () as u64);
}
