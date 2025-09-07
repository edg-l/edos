use alloc::vec::Vec;
use crossbeam_queue::SegQueue;
use spin::Once;

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

#[derive(Debug, Clone)]
pub struct AhciInterruptEvent {
    pub controller_pci_address: PciAddress, // Identify controller by PCI address
    pub port_mask: u32,                     // Bitmask of ports that triggered (bit N = port N)
    pub global_is: u32,                     // Global interrupt status from HBA
}

static AHCI_INTERRUPT_QUEUE: SegQueue<AhciInterruptEvent> = SegQueue::new();

static AHCI_INTERRUPT_PENDING: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn mark_interrupt_pending() {
    AHCI_INTERRUPT_PENDING.store(true, core::sync::atomic::Ordering::Release);
}

pub fn check_and_clear_interrupt_pending() -> bool {
    AHCI_INTERRUPT_PENDING.swap(false, core::sync::atomic::Ordering::AcqRel)
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
        // Check if any interrupt occurred
        if check_and_clear_interrupt_pending() {
            // Poll all controllers to see which one(s) triggered
            for controller in &mut controllers {
                if let Some(event) = controller.check_and_handle_interrupt() {
                    println!(
                        "AHCI interrupt on controller {:02x}:{:02x}.{} ports: {:#x}",
                        event.controller_pci_address.bus,
                        event.controller_pci_address.device,
                        event.controller_pci_address.function,
                        event.port_mask
                    );

                    // Process completed commands, wake waiting threads, etc.
                    // The actual port handling already happened in check_and_handle_interrupt()
                }
            }
        }

        // Yield to scheduler
        sched().thread_yield();
    }
}
