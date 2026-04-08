pub mod hid;
pub mod mass_storage;
pub mod xhci;

use crate::{interrupts::io::XHCI_DRIVER_THREAD_ID, thread::util::queue_spawn_kthread_named};

pub fn init() {
    XHCI_DRIVER_THREAD_ID.call_once(|| {
        queue_spawn_kthread_named("xhci", xhci::xhci_driver_main as *const () as u64)
    });
}
