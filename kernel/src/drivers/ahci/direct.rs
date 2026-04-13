use alloc::{sync::Arc, vec::Vec};
use spin::Once;

use crate::drivers::ahci::{AhciError, DeviceType, port::AhciPort};

/// Flat array of AHCI ports indexed by device_id.
static AHCI_PORTS: Once<Vec<Arc<AhciPort>>> = Once::new();

/// Initialize the direct access layer. Called once from ahci_driver_main after port discovery.
pub fn init(ports: Vec<Arc<AhciPort>>) {
    AHCI_PORTS.call_once(|| ports);
}

fn get_port(device_id: u64) -> Result<&'static Arc<AhciPort>, AhciError> {
    AHCI_PORTS
        .get()
        .and_then(|p| p.get(device_id as usize))
        .ok_or(AhciError::InvalidDevice)
}

/// Read sectors. For NCQ-capable ports, multiple threads can call this concurrently.
pub fn read_sectors(
    device_id: u64,
    lba: u64,
    sectors: u16,
    buffer: &mut [u8],
) -> Result<(), AhciError> {
    let port = get_port(device_id)?;
    port.read_sectors(lba, buffer, sectors)
}

/// Read multiple disjoint sector ranges concurrently (NCQ) or sequentially (fallback).
pub fn read_sectors_batch(
    device_id: u64,
    ranges: &mut [(u64, u16, &mut [u8])],
) -> Result<(), AhciError> {
    let port = get_port(device_id)?;
    port.read_sectors_batch(ranges)
}

/// Write sectors. For NCQ-capable ports, multiple threads can call this concurrently.
pub fn write_sectors(device_id: u64, lba: u64, data: &[u8], sectors: u16) -> Result<(), AhciError> {
    let port = get_port(device_id)?;
    port.write_sectors(lba, data, sectors)
}

/// Write sectors with Force Unit Access (bypasses drive write cache).
///
/// Falls back to a plain write followed by `flush_cache` on devices that do
/// not support FUA, so callers always get durability semantics even on QEMU.
pub fn write_sectors_fua(
    device_id: u64,
    lba: u64,
    data: &[u8],
    sectors: u16,
) -> Result<(), AhciError> {
    let port = get_port(device_id)?;
    match port.write_sectors_fua(lba, data, sectors) {
        Ok(()) => Ok(()),
        Err(AhciError::IoError) => {
            // Device does not support FUA; fall back to write + flush.
            port.write_sectors(lba, data, sectors)?;
            port.flush_cache()?;
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Flush cache. For NCQ-capable ports, drains NCQ before issuing FLUSH CACHE.
pub fn flush_cache(device_id: u64) -> Result<(), AhciError> {
    let port = get_port(device_id)?;
    port.flush_cache()
}

/// Wake all per-slot waiters for a device. Called by the interrupt dispatch thread.
pub fn wake_all_waiters(device_id: u64) {
    if let Some(port) = AHCI_PORTS.get().and_then(|p| p.get(device_id as usize)) {
        port.wake_all_slot_waiters();
    }
}

/// Whether the backing port is an ATAPI (CD/DVD) device. Used by the
/// partition scanner to skip CD-ROMs at boot — we have no ISO9660 driver
/// and probing them is artificially slow under QEMU emulation (~600ms on
/// the first command). When an ISO9660 driver lands, mount it explicitly
/// and detect_filesystem will work fine on demand.
pub fn is_atapi(device_id: u64) -> bool {
    get_port(device_id)
        .map(|p| p.device_type == DeviceType::Atapi)
        .unwrap_or(false)
}
