use core::ptr;

use alloc::{sync::Arc, vec::Vec};
use spin::Once;
use thiserror::Error;
use x86_64::instructions::hlt;

use crate::{
    drivers::{
        ahci::{controller::AhciController, port::AhciPort, structures::DeviceIdentifyInfo},
        dma::DmaError,
        pci::{
            pci_manager,
            structures::{PciAddress, PciDevice},
        },
    },
    log,
    thread::{
        mutex::BlockingMutex,
        runqueue::IO_PRIORITY,
        scheduler::{WakePriority, sched},
        thread::ThreadId,
        util::queue_spawn_kthread_named,
    },
};

pub mod api;
pub mod controller;
pub mod direct;

pub mod fis;
pub mod port;
pub mod structures;

#[derive(Debug, Error, Clone, Copy)]
pub enum AhciError {
    #[error("invalid device")]
    InvalidDevice,
    #[error(transparent)]
    DmaError(#[from] DmaError),
    #[error("port not ready")]
    PortNotReady,
    #[error("command timeout")]
    CommandTimeout,
    #[error("i/o error")]
    IoError,
    #[error("invalid command slot")]
    InvalidSlot,
    #[error("device is read-only")]
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceType {
    Ata,
    Atapi,
}

pub static AHCI_DRIVER_THREAD_ID: Once<ThreadId> = Once::new();

pub static DETECTED_DEVICES: Once<Vec<DetectedDevice>> = Once::new();

pub fn init() {
    AHCI_DRIVER_THREAD_ID
        .call_once(|| queue_spawn_kthread_named("ahci", ahci_driver_main as *const () as u64));
}

#[derive(Debug, Clone)]
pub struct DetectedDevice {
    pub id: u64,
    pub controller_pci_address: PciAddress,
    pub port_idx: usize,
    pub device_info: DeviceIdentifyInfo,
    pub device_type: DeviceType,
}

pub extern "C" fn ahci_driver_main() -> ! {
    let thread = sched().current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);

    let devices: Vec<PciDevice> = pci_manager().read().get_devices().to_vec();

    let mut controllers = Vec::new();

    // Find and initialize AHCI controllers
    for device in devices {
        if device.header.class_code == 0x01 && device.header.subclass == 0x06 {
            match AhciController::new(device) {
                Ok(controller) => {
                    log!("AHCI controller initialized successfully");
                    controllers.push(controller);
                }
                Err(e) => {
                    log!("Failed to initialize AHCI controller: {:?}", e);
                }
            }
        }
    }

    if controllers.is_empty() {
        log!("No AHCI controllers found");
        loop {
            hlt();
        }
    }

    let mut detected_devices: Vec<DetectedDevice> = Vec::new();

    let mut id = 0;
    for controller in &mut controllers {
        for port_idx in 0..controller.ports.len() {
            if let Some(port) = controller.ports[port_idx].as_mut() {
                let mut port = port.lock();
                match port.identify_device() {
                    Ok(device_info) => {
                        device_info.print_info(port_idx);
                        detected_devices.push(DetectedDevice {
                            id,
                            controller_pci_address: controller.pci_device.address,
                            port_idx,
                            device_info,
                            device_type: port.device_type,
                        });
                        id += 1;
                    }
                    Err(e) => {
                        log!("Failed to identify device on port {}: {:?}", port_idx, e);
                    }
                }
            }
        }
    }

    DETECTED_DEVICES.call_once(|| detected_devices.clone());

    // Initialize the direct-access layer with a flat port array indexed by device_id.
    {
        let mut direct_ports: Vec<Arc<BlockingMutex<AhciPort>>> =
            Vec::with_capacity(detected_devices.len());

        for device in &detected_devices {
            let port = controllers
                .iter()
                .find(|c| c.pci_device.address == device.controller_pci_address)
                .and_then(|c| c.ports.get(device.port_idx))
                .and_then(|p| p.clone())
                .expect("device port not found in controller");
            direct_ports.push(port);
        }

        direct::init(direct_ports);
    }

    loop {
        // Sleep until an MSI interrupt arrives.
        sched().thread_park_while(|| {
            !controllers.iter().any(|c| {
                let hba_is = unsafe { ptr::read_volatile(&raw const (*c.hba).is) };
                hba_is != 0
            })
        });

        // Dispatch HBA interrupts to direct callers.
        for controller in &controllers {
            let hba_is = unsafe { ptr::read_volatile(&(*controller.hba).is) };
            if hba_is == 0 {
                continue;
            }

            let mut pending_ports = hba_is;
            while pending_ports != 0 {
                let port_idx = pending_ports.trailing_zeros() as usize;
                pending_ports &= pending_ports - 1;

                let port_regs = unsafe { &mut (*controller.hba).ports[port_idx] };
                let port_is = unsafe { ptr::read_volatile(&port_regs.is) };
                if port_is != 0 {
                    unsafe { ptr::write_volatile(&mut port_regs.is, port_is) };

                    // Wake any thread blocked in direct::read_sectors / write_sectors.
                    if let Some(device) = detected_devices.iter().find(|d| {
                        d.controller_pci_address == controller.pci_device.address
                            && d.port_idx == port_idx
                    }) {
                        let waiter_tid = direct::get_waiter(device.id);
                        if waiter_tid != 0 {
                            sched().wake_thread(ThreadId(waiter_tid), WakePriority::Interrupt);
                        }
                    }
                }
            }

            unsafe { ptr::write_volatile(&mut (*controller.hba).is, hba_is) };
        }
    }
}
