use crate::{graphics, thread::util::queue_spawn_kthread_named};

pub mod hpet;
pub mod keyboard;
pub mod fpu;

pub fn init_drivers() {
    hpet::driver::init();
    unsafe { fpu::init_fpu() };
    queue_spawn_kthread_named("keyboard", keyboard::driver_main);
    queue_spawn_kthread_named("render", graphics::render_thread);
}
