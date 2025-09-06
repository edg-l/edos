use crate::{graphics, thread::util::queue_spawn_kthread_named};

pub mod fpu;
pub mod hpet;
pub mod keyboard;
pub mod pci;

pub fn init_drivers() {
    hpet::driver::init();
    unsafe { fpu::init_fpu() };
    pci::init();
    queue_spawn_kthread_named("keyboard", keyboard::driver_main);
    queue_spawn_kthread_named("render", graphics::render_thread);
}
