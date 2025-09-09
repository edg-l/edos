#![expect(unused)]

use core::{ptr, time::Duration};

use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    format,
    sync::Arc,
    vec::{self, Vec},
};
use spin::{Mutex, Once};
use thiserror::Error;
use x86_64::instructions::{hlt, interrupts::without_interrupts};

use crate::{
    drivers::{
        ahci::{
            api::send_request, controller::AhciController, port::AhciPort,
            structures::DeviceIdentifyInfo,
        },
        pci::{
            pci_manager,
            structures::{PciAddress, PciDevice},
        },
    },
    println,
    thread::{
        ThreadId,
        mailbox::{Mailbox, Request},
        scheduler::sched,
        util::queue_spawn_kthread_named,
    },
};

pub mod api;
pub mod command;
pub mod controller;
pub mod dma;
pub mod fis;
pub mod port;
pub mod structures;

#[derive(Debug, Error, Clone, Copy)]
pub enum AhciError {
    #[error("invalid device")]
    InvalidDevice,
    #[error("dma allocation failed")]
    DmaAllocationFailed,
    #[error("port not ready")]
    PortNotReady,
    #[error("command timeout")]
    CommandTimeout,
    #[error("i/o error")]
    IoError,
    #[error("invalid command slot")]
    InvalidSlot,
}

pub static AHCI_DRIVER_THREAD_ID: Once<ThreadId> = Once::new();

pub fn init() {
    AHCI_DRIVER_THREAD_ID.call_once(|| queue_spawn_kthread_named("ahci", ahci_driver_main as u64));
}

#[derive(Debug, Clone)]
pub struct DetectedDevice {
    pub id: u64,
    pub controller_pci_address: PciAddress,
    pub port_idx: usize,
    pub device_info: DeviceIdentifyInfo,
}

pub(super) static AHCI_REQUESTS: Once<Mailbox<AhciRequest, AhciResponse>> = Once::new();

#[derive(Debug)]
pub(super) enum AhciRequest {
    ListDevices,
    DeviceRequest {
        device_id: usize,
        command: Arc<Command>,
    },
    // Used internally
    GetDeviceMailbox(ThreadId),
    GetDevicePort(ThreadId),
}

#[derive(Debug, Clone)]
pub(super) enum Command {
    // todo: maybe accept a buffer ptr to fill instead of returning a vec
    Read {
        lba: u64,
        sectors: u16,
    },
    Write {
        lba: u64,
        data: Vec<u8>,
        sectors: u16,
    },
    Flush,
    Identify,
}

#[derive(Debug)]
pub(super) enum AhciResponse {
    Devices(Vec<DetectedDevice>),
    ReadResult {
        data: Result<Vec<u8>, AhciError>,
    },
    IdentifyResult {
        info: Result<DeviceIdentifyInfo, AhciError>,
    },
    Result(Result<(), AhciError>),
    DeviceMailbox(Option<PortMailbox>),
    DevicePort(Option<Arc<Mutex<AhciPort>>>),
}

type PortMailbox = Mailbox<(Arc<Command>, Request<AhciRequest, AhciResponse>), AhciResponse>;

pub extern "C" fn ahci_driver_main() -> ! {
    let tid = sched().current_id();

    let requests = AHCI_REQUESTS.call_once(|| Mailbox::new(tid));

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

    let mut detected_devices: Vec<DetectedDevice> = Vec::new();

    let mut device_mailboxes: Vec<PortMailbox> = Vec::new();

    let mut id = 0;
    for controller in &mut controllers {
        for port_idx in 0..controller.ports.len() {
            if let Some(port) = controller.ports[port_idx].as_mut() {
                match port.lock().identify_device() {
                    Ok(device_info) => {
                        device_info.print_info(port_idx);
                        detected_devices.push(DetectedDevice {
                            id,
                            controller_pci_address: controller.pci_device.address,
                            port_idx,
                            device_info,
                        });
                        id += 1;
                    }
                    Err(e) => {
                        println!("Failed to identify device on port {}: {:?}", port_idx, e);
                    }
                }
            }
        }
    }

    let mut port_map = BTreeMap::new();
    let mut port_map_reverse = BTreeMap::new();

    for device in &detected_devices {
        let worker_tid = queue_spawn_kthread_named(
            &format!("ahci-port-{}-{}", device.id, device.port_idx),
            port_worker_thread as u64,
        );
        port_map.insert(worker_tid.clone(), device.id);
        port_map_reverse.insert(device.id, worker_tid.clone());
        device_mailboxes.push(Mailbox::new(worker_tid));
    }

    // The AHCI main thread job is to route requests to ports.

    loop {
        // Check and wake relevant port worker threads.
        without_interrupts(|| {
            for controller in &controllers {
                // Read HBA interrupt status register
                let hba_is = unsafe { ptr::read_volatile(&(*controller.hba).is) };

                if hba_is == 0 {
                    continue; // No interrupts on this controller
                }

                // Process only ports with pending interrupts using bit manipulation
                let mut pending_ports = hba_is;
                while pending_ports != 0 {
                    // Find the lowest set bit (next port with interrupt)
                    let port_idx = pending_ports.trailing_zeros() as usize;

                    // Clear this bit for next iteration
                    pending_ports &= pending_ports - 1;

                    // Process this port's interrupt
                    let port_regs = unsafe { &mut (*controller.hba).ports[port_idx] };
                    let port_is = unsafe { ptr::read_volatile(&port_regs.is) };

                    if port_is != 0 {
                        // Clear the port interrupt status
                        unsafe { ptr::write_volatile(&mut port_regs.is, port_is) };

                        // Find the device for this controller+port combination
                        if let Some(device) = detected_devices.iter().find(|d| {
                            d.controller_pci_address == controller.pci_device.address
                                && d.port_idx == port_idx
                        }) {
                            // Wake the worker thread for this device
                            if let Some(worker_tid) = port_map_reverse.get(&device.id) {
                                sched().thread_wake(worker_tid.clone());
                            }
                        }
                    }
                }

                // Clear HBA interrupt status for processed ports
                unsafe { ptr::write_volatile(&mut (*controller.hba).is, hba_is) };
            }
        });

        while let Some(req) = requests.pop_request() {
            match &req.message {
                AhciRequest::ListDevices => {
                    req.answer(AhciResponse::Devices(detected_devices.clone()));
                }
                AhciRequest::DeviceRequest { device_id, command } => {
                    if let Some(mb) = device_mailboxes.get(*device_id) {
                        mb.send((command.clone(), req));
                    }
                }
                AhciRequest::GetDeviceMailbox(thread_id) => {
                    let id = port_map.get(thread_id);

                    if let Some(id) = id {
                        let mailbox = (device_mailboxes.get((*id) as usize)).cloned();

                        req.answer(AhciResponse::DeviceMailbox(mailbox));
                    } else {
                        req.answer(AhciResponse::DeviceMailbox(None));
                    }
                }
                AhciRequest::GetDevicePort(worker_tid) => {
                    let mut found = false;
                    let info = port_map.get(worker_tid);

                    if let Some(info) = port_map.get(worker_tid) {
                        let device = &detected_devices[*info as usize];

                        for controller in &controllers {
                            if controller.pci_device.address == device.controller_pci_address {
                                let port = controller.ports.get(device.port_idx).cloned().flatten();
                                req.answer(AhciResponse::DevicePort(port));
                                found = true;
                                break;
                            }
                        }
                    }

                    if !found {
                        req.answer(AhciResponse::DevicePort(None));
                    }
                }
            }
        }

        // Wait for more requests
        sched().thread_park();
    }
}

extern "C" fn port_worker_thread() -> ! {
    let tid = sched().current_id();

    let mailbox = {
        loop {
            let mailbox = send_request(
                AhciRequest::GetDeviceMailbox(tid.clone()),
                Duration::from_secs(10),
            );

            if let AhciResponse::DeviceMailbox(Some(mailbox)) = mailbox {
                break mailbox;
            }
        }
    };

    let port = {
        loop {
            let port = send_request(
                AhciRequest::GetDevicePort(tid.clone()),
                Duration::from_secs(10),
            );

            if let AhciResponse::DevicePort(Some(port)) = port {
                break port;
            }
        }
    };

    loop {
        while let Some(req) = mailbox.pop_request() {
            let message = &*req.message.0;
            let caller = req.message.1;
            match message {
                Command::Read { lba, sectors } => {
                    println!("Got read request, lba={lba}, sectors={sectors}");
                    let mut buffer = alloc::vec![0; (*sectors) as usize * 512];
                    let result = port.lock().read_sectors(*lba, &mut buffer, *sectors);

                    match result {
                        Ok(_) => caller.answer(AhciResponse::ReadResult { data: Ok(buffer) }),
                        Err(e) => caller.answer(AhciResponse::ReadResult { data: Err(e) }),
                    }
                }
                Command::Write { lba, data, sectors } => {
                    println!(
                        "Got write request, lba={lba}, sectors={sectors}, data_len={}",
                        data.len()
                    );
                    let result = port.lock().write_sectors(*lba, data, *sectors);
                    caller.answer(AhciResponse::Result(result));
                }
                Command::Flush => {
                    println!("Got flush request");
                    let result = port.lock().flush_cache();
                    caller.answer(AhciResponse::Result(result));
                }
                Command::Identify => {
                    println!("Got identify request");
                    let result = port.lock().identify_device();
                    caller.answer(AhciResponse::IdentifyResult { info: result });
                }
            }
        }

        sched().thread_park();
    }
}
