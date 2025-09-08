use core::time::Duration;

use alloc::vec::Vec;

use crate::{
    drivers::ahci::{AHCI_REQUESTS, AhciRequest, AhciResponse, DetectedDevice},
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
