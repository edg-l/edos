//! Raw block devices exposed through devfs.
//!
//! One node per device, not per partition: `edos-install` writes the partition
//! table itself and computes offsets, so partition nodes would only add a
//! second naming scheme to keep consistent.
//!
//! All access goes through the block page cache. Going around it would leave
//! stale pages under the same `(device_id, page_block_idx)` key that the
//! filesystem driver reads through when the new partition is mounted moments
//! later, which is the classic "the install looked fine but does not boot"
//! failure.

use alloc::{
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use crate::{
    drivers::{ahci::api::list_devices, block_io, ramdisk::RAMDISK_DEVICE_ID},
    fs::{
        block_page_cache::BlockPageCache,
        devfs::{DevFsDevice, DevFsError, register_device_str},
        vfs,
    },
    log,
};

/// Flush every dirty page for this device and issue a hardware cache flush.
/// `arg` is ignored.
pub const BLOCK_IOCTL_FLUSH: u64 = 0x424B_0001;
/// Returns the device capacity in 512-byte sectors.
pub const BLOCK_IOCTL_SECTOR_COUNT: u64 = 0x424B_0002;
/// Re-read this device's partition table. Returns the number of partitions
/// found. Fails with `EBUSY` if the device backs a mounted filesystem.
pub const BLOCK_IOCTL_RESCAN: u64 = 0x424B_0003;
/// Returns 1 while a mounted filesystem is backed by this device, else 0.
pub const BLOCK_IOCTL_IS_MOUNTED: u64 = 0x424B_0004;

const SECTOR_SIZE: usize = 512;

struct BlockDevNode {
    device_id: u64,
}

impl BlockDevNode {
    fn sectors(&self) -> u64 {
        block_io::lookup(self.device_id)
            .map(|d| d.sector_count())
            .unwrap_or(0)
    }

    /// Reject anything that runs off the end of the device.
    ///
    /// Access is byte-granular rather than sector-granular: it goes through
    /// the block page cache, which reads a page before patching it, so a short
    /// write updates exactly the bytes given and leaves the rest of the sector
    /// alone. Nothing is silently rounded outward.
    fn check_range(&self, offset: usize, len: usize) -> Result<(), DevFsError> {
        if len == 0 {
            return Err(DevFsError::InvalidPath);
        }
        let end = (offset as u64)
            .checked_add(len as u64)
            .ok_or(DevFsError::InvalidPath)?;
        let capacity = self.sectors() * SECTOR_SIZE as u64;
        if capacity == 0 || end > capacity {
            return Err(DevFsError::InvalidPath);
        }
        Ok(())
    }

    /// True while any mounted filesystem is backed by this device.
    fn is_mounted(&self) -> bool {
        vfs::list_mounts()
            .iter()
            .any(|m| m.device_id as u64 == self.device_id && m.filesystem.is_device_backed())
    }
}

impl DevFsDevice for BlockDevNode {
    fn read(&self, offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        self.check_range(offset, count)?;
        BlockPageCache::global()
            .read_bytes(self.device_id, offset as u64, count)
            .map_err(|_| DevFsError::IoError)
    }

    fn write(&self, offset: usize, data: &[u8]) -> Result<usize, DevFsError> {
        self.check_range(offset, data.len())?;

        // Writing under a live mount corrupts the running system. There is no
        // override flag: a caller that wants the disk back can unmount it.
        if self.is_mounted() {
            return Err(DevFsError::Busy);
        }

        BlockPageCache::global()
            .write_bytes(self.device_id, offset as u64, data)
            .map(|()| data.len())
            .map_err(|_| DevFsError::IoError)
    }

    fn ioctl(&self, request: u64, _arg: u64) -> Result<u64, DevFsError> {
        match request {
            BLOCK_IOCTL_FLUSH => BlockPageCache::global()
                .flush_device(self.device_id)
                .map(|()| 0)
                .map_err(|_| DevFsError::IoError),
            BLOCK_IOCTL_SECTOR_COUNT => Ok(self.sectors()),
            BLOCK_IOCTL_IS_MOUNTED => Ok(self.is_mounted() as u64),
            BLOCK_IOCTL_RESCAN => crate::fs::api::rescan_partitions(self.device_id)
                .map(|n| n as u64)
                .map_err(|e| match e {
                    crate::fs::Error::Busy => DevFsError::Busy,
                    _ => DevFsError::IoError,
                }),
            _ => Err(DevFsError::Unsupported),
        }
    }

    fn size(&self) -> u64 {
        self.sectors() * SECTOR_SIZE as u64
    }
}

/// devfs name for a block device id.
///
/// AHCI ports and USB mass storage share the `sd*` series in id order, which
/// is stable because AHCI is fully probed before USB enumeration can add
/// anything. The ramdisk is deliberately outside that series: it is registered
/// before USB storage but has a higher id, so it would otherwise rename itself
/// when a USB stick appeared.
fn device_name(device_id: u64) -> Option<String> {
    if device_id == RAMDISK_DEVICE_ID {
        return Some("ram0".to_string());
    }

    let index = if device_id >= 1000 {
        list_devices().len() as u64 + (device_id - 1000)
    } else {
        device_id
    };

    if index >= 26 {
        return None;
    }
    Some(format!("sd{}", (b'a' + index as u8) as char))
}

/// Register a devfs node for every block device that does not have one yet.
///
/// Called after the boot-time partition scan and again whenever a device
/// appears later (USB storage), so nodes exist for disks that carry no
/// partition table at all.
pub fn register_all() {
    for device_id in block_io::list() {
        let Some(name) = device_name(device_id) else {
            log!("devfs: no name left for block device {device_id}");
            continue;
        };
        let node = Arc::new(BlockDevNode { device_id }) as Arc<dyn DevFsDevice>;
        match register_device_str(&format!("/{name}"), node) {
            Ok(()) => log!("devfs: /dev/{name} -> block device {device_id}"),
            // Already registered by an earlier call; nothing to do.
            Err(DevFsError::AlreadyExists) => {}
            Err(e) => log!("devfs: failed to register /dev/{name}: {e:?}"),
        }
    }
}
