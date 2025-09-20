#![expect(unused)]

use alloc::{boxed::Box, vec::Vec};

use crate::{
    drivers::{
        pci::{pci_manager, structures::PciDevice},
        vga::controller::VgaController,
    },
    log,
    thread::{scheduler::sched, util::queue_spawn_kthread_named_arg},
};

pub mod controller;
pub mod error;
pub mod structures;

pub fn init() {
    let manager = pci_manager().read();

    for device in manager.get_devices() {
        if device.header.class_code == 0x03 && device.header.subclass == 0x0 {
            queue_spawn_kthread_named_arg(
                "vga",
                vga_thread as u64,
                Box::into_raw(Box::new(*device)).cast(),
            );
            break;
        }
    }
}

extern "C" fn vga_thread(device: *mut PciDevice) {
    let device = unsafe { Box::from_raw(device) };
    let capabilities: Vec<_> = pci_manager().write().capabilities(device.address).collect();
    log!(
        "Starting vga thread, address={:?}, caps={:?}",
        device.address,
        capabilities
    );

    let controller = VgaController::new(*device).unwrap();

    loop {
        sched().thread_park();
    }
}
