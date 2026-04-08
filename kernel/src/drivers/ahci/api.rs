use alloc::vec::Vec;

use crate::drivers::ahci::{DETECTED_DEVICES, DetectedDevice};

pub fn list_devices() -> Vec<DetectedDevice> {
    DETECTED_DEVICES.wait().clone()
}
