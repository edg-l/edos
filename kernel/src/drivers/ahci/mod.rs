#![expect(unused)]

use core::time::Duration;

use alloc::{
    boxed::Box,
    collections::btree_map::BTreeMap,
    format,
    sync::Arc,
    vec::{self, Vec},
};
use spin::{Mutex, Once};
use x86_64::instructions::hlt;

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

#[derive(Debug)]
pub enum AhciError {
    InvalidDevice,
    DmaAllocationFailed,
    PortNotReady,
    CommandTimeout,
    IoError,
    InvalidSlot,
}

pub static AHCI_DRIVER_THREAD_ID: Once<ThreadId> = Once::new();

pub fn init() {
    AHCI_DRIVER_THREAD_ID.call_once(|| queue_spawn_kthread_named("ahci", ahci_driver_main));
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

pub fn ahci_driver_main() -> ! {
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

    // Add this code after the detected_devices.push() in ahci_driver_main() in mod.rs

    // Test reading GPT header from first detected device
    if !detected_devices.is_empty() {
        use alloc::string::String;

        let mut output = String::new();

        // Get the first controller and port with a device
        if let Some(first_device) = detected_devices.first() {
            // Find the controller
            for controller in &mut controllers {
                if controller.pci_device.address == first_device.controller_pci_address
                    && let Some(port) = controller.ports[first_device.port_idx].as_mut()
                {
                    // Read GPT header (LBA 1, 1 sector = 512 bytes)
                    let mut gpt_buffer = [0u8; 512];

                    output.push_str("Reading GPT header from LBA 1...\n");
                    let mut port = port.lock();
                    match port.read_sectors(1, &mut gpt_buffer, 1) {
                        Ok(()) => {
                            output.push_str("Successfully read GPT header\n");

                            // Check for GPT signature "EFI PART"
                            let signature = &gpt_buffer[0..8];

                            println!("Read signature: {signature:?}");
                            if signature == b"EFI PART" {
                                output.push_str("Valid GPT signature found\n");

                                // Parse some basic GPT header fields
                                let revision = u32::from_le_bytes([
                                    gpt_buffer[8],
                                    gpt_buffer[9],
                                    gpt_buffer[10],
                                    gpt_buffer[11],
                                ]);
                                let header_size = u32::from_le_bytes([
                                    gpt_buffer[12],
                                    gpt_buffer[13],
                                    gpt_buffer[14],
                                    gpt_buffer[15],
                                ]);
                                let num_partition_entries = u32::from_le_bytes([
                                    gpt_buffer[80],
                                    gpt_buffer[81],
                                    gpt_buffer[82],
                                    gpt_buffer[83],
                                ]);

                                output.push_str(&alloc::format!("GPT Revision: {:#x}\n", revision));
                                output.push_str(&alloc::format!(
                                    "Header Size: {} bytes\n",
                                    header_size
                                ));
                                output.push_str(&alloc::format!(
                                    "Number of partition entries: {}\n",
                                    num_partition_entries
                                ));
                                output.push('\n');
                            } else {
                                output.push_str(
                                    "No valid GPT signature found. Raw signature bytes:\n",
                                );
                                output.push_str("Signature: ");
                                for &byte in signature {
                                    output.push_str(&alloc::format!("{:02x} ", byte));
                                }
                                output.push('\n');

                                // Check if it might be MBR instead
                                if gpt_buffer[510] == 0x55 && gpt_buffer[511] == 0xAA {
                                    output.push_str(
                                        "This appears to be an MBR disk (boot signature found)\n",
                                    );
                                } else {
                                    output.push_str("Unknown partition table format\n");
                                }
                            }
                        }
                        Err(e) => {
                            output
                                .push_str(&alloc::format!("Failed to read GPT header: {:?}\n", e));
                        }
                    }

                    // Also test reading multiple sectors (first 2 sectors)
                    output.push_str("\nTesting multi-sector read (LBA 0-1, 2 sectors)...\n");
                    let mut multi_buffer = [0u8; 1024]; // 2 sectors

                    match port.read_sectors(0, &mut multi_buffer, 2) {
                        Ok(()) => {
                            output.push_str("Successfully read 2 sectors!\n");
                            output.push_str(&alloc::format!(
                                "MBR signature at end of first sector: {:02x} {:02x}\n",
                                multi_buffer[510],
                                multi_buffer[511]
                            ));
                            output.push_str(&alloc::format!(
                                "GPT signature in second sector: {:?}\n",
                                core::str::from_utf8(&multi_buffer[512..520]).unwrap_or("invalid")
                            ));
                        }
                        Err(e) => {
                            output.push_str(&alloc::format!(
                                "Failed to read multiple sectors: {:?}\n",
                                e
                            ));
                        }
                    }

                    break;
                }
            }
        }

        println!("{}", output);
    }

    let mut port_map = BTreeMap::new();
    let mut port_map_reverse = BTreeMap::new();

    for device in &detected_devices {
        let worker_tid = queue_spawn_kthread_named(
            &format!("ahci-port-{}-{}", device.id, device.port_idx),
            port_worker_thread,
        );
        port_map.insert(worker_tid.clone(), device.id);
        port_map_reverse.insert(device.id, worker_tid.clone());
        device_mailboxes.push(Mailbox::new(worker_tid));
    }

    // The AHCI main thread job is to route requests to ports.

    loop {
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

fn port_worker_thread() -> ! {
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
                    let mut buffer = alloc::vec![0; (*sectors) as usize * 512];
                    let result = port.lock().read_sectors(*lba, &mut buffer, *sectors);

                    match result {
                        Ok(_) => caller.answer(AhciResponse::ReadResult { data: Ok(buffer) }),
                        Err(e) => caller.answer(AhciResponse::ReadResult { data: Err(e) }),
                    }
                }
                Command::Write { lba, data, sectors } => {
                    let result = port.lock().write_sectors(*lba, data, *sectors);
                    caller.answer(AhciResponse::Result(result));
                }
                Command::Flush => {
                    let result = port.lock().flush_cache();
                    caller.answer(AhciResponse::Result(result));
                }
                Command::Identify => {
                    let result = port.lock().identify_device();
                    caller.answer(AhciResponse::IdentifyResult { info: result });
                }
            }
        }

        sched().thread_wait_timeout(Duration::from_secs(1));
    }
}
