use crate::thread::{util::queue_spawn_kthread};

pub mod keyboard;

pub fn init_drivers() {

    queue_spawn_kthread(keyboard::driver_main);
}
