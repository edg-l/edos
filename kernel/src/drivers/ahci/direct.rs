use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};
use spin::{Mutex, Once};

use crate::drivers::ahci::{AhciError, port::AhciPort};

/// Flat array of AHCI ports indexed by device_id.
static AHCI_PORTS: Once<Vec<Arc<Mutex<AhciPort>>>> = Once::new();

/// Per-device_id atomic storing the TID of the thread currently waiting for DMA.
/// 0 means no thread is waiting. Indexed by device_id.
static AHCI_PORT_WAITERS: Once<Vec<AtomicU64>> = Once::new();

/// Mapping from (controller_index, port_idx) to device_id.
/// Used by the interrupt dispatcher to find the right waiter.
static PORT_TO_DEVICE: Once<Vec<(usize, usize, u64)>> = Once::new();

/// Initialize the direct access layer. Called once from ahci_driver_main after port discovery.
pub fn init(ports: Vec<Arc<Mutex<AhciPort>>>) {
    let count = ports.len();
    AHCI_PORTS.call_once(|| ports);
    AHCI_PORT_WAITERS.call_once(|| {
        let mut v = Vec::with_capacity(count);
        for _ in 0..count {
            v.push(AtomicU64::new(0));
        }
        v
    });
}

/// Set the port-to-device mapping for interrupt dispatch.
pub fn set_port_mapping(mapping: Vec<(usize, usize, u64)>) {
    PORT_TO_DEVICE.call_once(|| mapping);
}

/// Look up the device_id for a given (controller_index, port_idx) pair.
/// Returns None if not found.
pub fn device_id_for_port(controller_idx: usize, port_idx: usize) -> Option<u64> {
    PORT_TO_DEVICE.get().and_then(|map| {
        map.iter()
            .find(|(cidx, pidx, _)| *cidx == controller_idx && *pidx == port_idx)
            .map(|(_, _, did)| *did)
    })
}

/// Get the waiter TID for a device. Returns 0 if nobody is waiting.
pub fn get_waiter(device_id: u64) -> u64 {
    AHCI_PORT_WAITERS
        .get()
        .and_then(|w| w.get(device_id as usize))
        .map(|a| a.load(Ordering::Acquire))
        .unwrap_or(0)
}

/// Read sectors directly. The calling thread blocks until DMA completes.
pub fn read_sectors(
    device_id: u64,
    lba: u64,
    sectors: u16,
    buffer: &mut [u8],
) -> Result<(), AhciError> {
    let ports = AHCI_PORTS.get().ok_or(AhciError::InvalidDevice)?;
    let port = ports
        .get(device_id as usize)
        .ok_or(AhciError::InvalidDevice)?;
    let waiters = AHCI_PORT_WAITERS.get().ok_or(AhciError::InvalidDevice)?;
    let waiter = waiters
        .get(device_id as usize)
        .ok_or(AhciError::InvalidDevice)?;

    let tid = crate::thread::scheduler::sched()
        .current_thread()
        .unwrap()
        .id;
    waiter.store(tid.0, Ordering::Release);

    let mut port_guard = port.lock();
    let result = port_guard.read_sectors(lba, buffer, sectors);

    waiter.store(0, Ordering::Release);
    result
}

/// Write sectors directly. The calling thread blocks until DMA completes.
pub fn write_sectors(device_id: u64, lba: u64, data: &[u8], sectors: u16) -> Result<(), AhciError> {
    let ports = AHCI_PORTS.get().ok_or(AhciError::InvalidDevice)?;
    let port = ports
        .get(device_id as usize)
        .ok_or(AhciError::InvalidDevice)?;
    let waiters = AHCI_PORT_WAITERS.get().ok_or(AhciError::InvalidDevice)?;
    let waiter = waiters
        .get(device_id as usize)
        .ok_or(AhciError::InvalidDevice)?;

    let tid = crate::thread::scheduler::sched()
        .current_thread()
        .unwrap()
        .id;
    waiter.store(tid.0, Ordering::Release);

    let mut port_guard = port.lock();
    let result = port_guard.write_sectors(lba, data, sectors);

    waiter.store(0, Ordering::Release);
    result
}

/// Flush cache directly.
pub fn flush_cache(device_id: u64) -> Result<(), AhciError> {
    let ports = AHCI_PORTS.get().ok_or(AhciError::InvalidDevice)?;
    let port = ports
        .get(device_id as usize)
        .ok_or(AhciError::InvalidDevice)?;

    let mut port_guard = port.lock();
    port_guard.flush_cache()
}
