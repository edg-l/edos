use crate::thread::util::queue_spawn_kthread_named;

pub mod hpet;
pub mod keyboard;

pub fn init_drivers() {
    hpet::driver::init();
    queue_spawn_kthread_named("keyboard", keyboard::driver_main);
}
