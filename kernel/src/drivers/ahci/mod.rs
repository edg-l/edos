use alloc::vec::Vec;
use crossbeam_queue::SegQueue;
use spin::Once;
use x86_64::instructions::hlt;

use crate::{
    drivers::{
        ahci::controller::AhciController,
        pci::{
            pci_manager,
            structures::{PciAddress, PciDevice},
        },
    },
    println,
    thread::{ThreadId, scheduler::sched, util::queue_spawn_kthread_named},
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

pub static AHCI_DRIVER_THREAD_ID: Once<ThreadId> = Once::new();

pub fn init() {
    AHCI_DRIVER_THREAD_ID.call_once(|| queue_spawn_kthread_named("ahci", ahci_driver_main));
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
        loop {
            hlt();
        }
    }

    for controller in &mut controllers {
        for port_idx in 0..controller.ports.len() {
            if let Some(port) = controller.ports[port_idx].as_mut() {
                println!("Testing IDENTIFY command on controller port {}", port_idx);
                match port.identify_device() {
                    Ok(identify_data) => {
                        identify_data.print_info(port_idx);
                    }
                    Err(e) => {
                        println!("Failed to identify device on port {}: {:?}", port_idx, e);
                    }
                }
            }
        }
    }

    loop {
        // Yield to scheduler
        sched().thread_yield();
    }
}
