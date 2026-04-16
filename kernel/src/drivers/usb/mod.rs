pub mod block_api;
pub mod hid;
pub mod mass_storage;
pub mod xhci;

use crate::{
    interrupts::io::XHCI_DRIVER_THREAD_ID,
    thread::{thread::get_thread_weak, util::queue_spawn_kthread_named},
};

pub fn init() {
    XHCI_DRIVER_THREAD_ID.call_once(|| {
        let tid = queue_spawn_kthread_named("xhci", xhci::xhci_driver_main as *const () as u64);
        get_thread_weak(tid).expect("xhci kthread vanished before call_once")
    });
}
