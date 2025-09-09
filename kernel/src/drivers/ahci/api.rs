use core::time::Duration;

use alloc::{sync::Arc, vec::Vec};

use crate::{
    drivers::ahci::{
        AHCI_REQUESTS, AhciError, AhciRequest, AhciResponse, Command, DetectedDevice,
        structures::DeviceIdentifyInfo,
    },
    thread::{ThreadId, scheduler::sched},
};

pub(super) fn send_request(request: AhciRequest, timeout: Duration) -> AhciResponse {
    let requests = {
        loop {
            if let Some(req) = AHCI_REQUESTS.get() {
                break req;
            }
            sched().thread_yield();
        }
    };

    let response = requests.send(request);

    loop {
        match response.receive_timeout(timeout) {
            Ok(res) => break res,
            Err(_) => continue,
        }
    }
}

pub fn list_devices() -> Vec<DetectedDevice> {
    let AhciResponse::Devices(devices) =
        send_request(AhciRequest::ListDevices, Duration::from_secs(1))
    else {
        unreachable!()
    };
    devices
}

/// Read sectors from a device
///
/// # Arguments
/// * `device_id` - The device ID from list_devices()
/// * `lba` - Logical block address to start reading from
/// * `sectors` - Number of sectors to read (each sector is 512 bytes)
///
/// # Returns
/// Vector containing the read data (sectors * 512 bytes)
pub fn read_sectors(device_id: u64, lba: u64, sectors: u16) -> Result<Vec<u8>, AhciError> {
    let command = Command::Read { lba, sectors };

    let response = send_request(
        AhciRequest::DeviceRequest {
            device_id: device_id,
            command: Arc::new(command),
        },
        Duration::from_secs(10),
    );

    match response {
        AhciResponse::ReadResult { data } => data,
        _ => unreachable!(),
    }
}

/// Write sectors to a device
///
/// # Arguments
/// * `device_id` - The device ID from list_devices()
/// * `lba` - Logical block address to start writing to
/// * `data` - Data to write (must be sectors * 512 bytes)
/// * `sectors` - Number of sectors to write (each sector is 512 bytes)
pub fn write_sectors(
    device_id: u64,
    lba: u64,
    data: Vec<u8>,
    sectors: u16,
) -> Result<(), AhciError> {
    let command = Command::Write { lba, data, sectors };

    let response = send_request(
        AhciRequest::DeviceRequest {
            device_id,
            command: Arc::new(command),
        },
        Duration::from_secs(10),
    );

    match response {
        AhciResponse::Result(result) => result,
        _ => unreachable!(),
    }
}

/// Flush the device cache
///
/// # Arguments
/// * `device_id` - The device ID from list_devices()
pub fn flush_cache(device_id: u64) -> Result<(), AhciError> {
    let command = Command::Flush;

    let response = send_request(
        AhciRequest::DeviceRequest {
            device_id,
            command: Arc::new(command),
        },
        Duration::from_secs(5),
    );

    match response {
        AhciResponse::Result(result) => result,
        _ => unreachable!(),
    }
}

/// Get device identification information
///
/// # Arguments
/// * `device_id` - The device ID from list_devices()
///
/// # Returns
/// Device identification information including model, serial, etc.
pub fn identify_device(device_id: u64) -> Result<DeviceIdentifyInfo, AhciError> {
    let command = Command::Identify;

    let response = send_request(
        AhciRequest::DeviceRequest {
            device_id,
            command: Arc::new(command),
        },
        Duration::from_secs(5),
    );

    match response {
        AhciResponse::IdentifyResult { info } => info,
        _ => unreachable!(),
    }
}
