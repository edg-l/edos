use core::ptr;

use alloc::{sync::Arc, vec::Vec};
use spin::Once;
use thiserror::Error;
use x86_64::instructions::hlt;

use crate::{
    drivers::{
        ahci::{controller::AhciController, port::AhciPort, structures::DeviceIdentifyInfo},
        pci::{
            pci_manager,
            structures::{PciAddress, PciDevice},
        },
    },
    log,
    thread::{
        runqueue::IO_PRIORITY, scheduler::sched, thread::ThreadId, util::queue_spawn_kthread_named,
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
    DmaError(#[from] crate::drivers::dma::DmaError),
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
    #[expect(unused)]
    pub device_info: DeviceIdentifyInfo,
    #[expect(unused)]
    pub device_type: DeviceType,
    #[expect(unused)]
    pub supports_ncq: bool,
    #[expect(unused)]
    pub ncq_depth: u8,
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

    // Identify devices and initialize I/O pools.
    // Ports are still owned by controllers (not yet Arc-wrapped).
    let mut id = 0;
    for controller in &mut controllers {
        for port_idx in 0..controller.ports.len() {
            if let Some(port) = controller.ports[port_idx].as_mut() {
                match port.identify_device() {
                    Ok(device_info) => {
                        device_info.print_info(port_idx);

                        // NCQ enabled only if both HBA and device support it
                        let supports_ncq = controller.supports_ncq
                            && device_info.supports_ncq
                            && port.device_type == DeviceType::Ata;
                        let ncq_depth = if supports_ncq {
                            device_info
                                .ncq_queue_depth
                                .min(controller.num_command_slots)
                        } else {
                            0
                        };

                        // Allocate per-slot DMA pools and command tables.
                        if let Err(e) = port.init_io_pools(ncq_depth) {
                            log!("Failed to init I/O pools for port {}: {:?}", port_idx, e);
                            continue;
                        }

                        detected_devices.push(DetectedDevice {
                            id,
                            controller_pci_address: controller.pci_device.address,
                            port_idx,
                            device_info,
                            device_type: port.device_type,
                            supports_ncq,
                            ncq_depth,
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

    // Move ports out of controllers, wrap in Arc, and pass to the direct layer.
    {
        let mut direct_ports: Vec<Arc<AhciPort>> = Vec::with_capacity(detected_devices.len());

        for device in &detected_devices {
            let port = controllers
                .iter_mut()
                .find(|c| c.pci_device.address == device.controller_pci_address)
                .and_then(|c| c.ports.get_mut(device.port_idx))
                .and_then(|p| p.take())
                .expect("device port not found in controller");
            direct_ports.push(Arc::new(port));
        }

        direct::init(direct_ports);
    }

    // Interrupt dispatch loop.
    // MSI fires -> hardware ISR wakes this thread -> we dispatch per-slot wakeups.
    loop {
        sched().thread_park_while(|| {
            !controllers.iter().any(|c| {
                let hba_is = unsafe { ptr::read_volatile(&raw const (*c.hba).is) };
                hba_is != 0
            })
        });

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

                    // Wake ALL per-slot waiters for this port. Each thread's
                    // park condition re-checks SACT/CI to determine if its
                    // specific command completed. Spurious wakes are harmless.
                    if let Some(device) = detected_devices.iter().find(|d| {
                        d.controller_pci_address == controller.pci_device.address
                            && d.port_idx == port_idx
                    }) {
                        direct::wake_all_waiters(device.id);
                    }
                }
            }

            unsafe { ptr::write_volatile(&mut (*controller.hba).is, hba_is) };
        }
    }
}
