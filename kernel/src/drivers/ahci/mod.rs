use alloc::vec::Vec;
use x86_64::instructions::hlt;

use crate::{drivers::pci::{pci_manager, structures::PciDevice}, thread::util::queue_spawn_kthread_named};

pub mod command;
pub mod controller;
pub mod fis;
pub mod port;



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

    loop {
        hlt();
    }
}
