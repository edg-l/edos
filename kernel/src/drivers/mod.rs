use crate::thread::util::queue_spawn_kthread;

pub mod hpet;
pub mod keyboard;

pub fn init_drivers() {
    hpet::driver::init();
    queue_spawn_kthread(keyboard::driver_main);
}
