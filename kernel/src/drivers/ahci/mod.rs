use alloc::vec::Vec;
use x86_64::instructions::hlt;

use crate::{
    drivers::{
        ahci::controller::AhciController,
        pci::{pci_manager, structures::PciDevice},
    },
    println,
    thread::util::queue_spawn_kthread_named,
};

pub mod command;
pub mod controller;
pub mod fis;
pub mod port;
pub mod structures;

#[derive(Debug)]
pub enum AhciError {
    InvalidDevice,
    DmaAllocationFailed,
    PortNotReady,
    CommandTimeout,
    IoError,
}

pub fn init() {
    queue_spawn_kthread_named("ahci", ahci_driver_main);
}

pub fn ahci_driver_main() -> ! {
    let devices: Vec<PciDevice> = pci_manager().read().get_devices().to_vec();

    let mut controllers = Vec::new();

    // Find and initialize AHCI controllers
    for device in devices {
        if device.header.class_code == 0x01 && device.header.subclass == 0x06 {
            match AhciController::new(device) {
                Ok(controller) => {
                    println!("AHCI controller initialized successfully");
                    controllers.push(controller);
                }
                Err(e) => {
                    println!("Failed to initialize AHCI controller: {:?}", e);
                }
            }
        }
    }

    if controllers.is_empty() {
        println!("No AHCI controllers found");
    }

    loop {
        hlt();
    }
}
