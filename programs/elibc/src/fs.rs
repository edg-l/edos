use alloc::vec::Vec;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::{ffi::CStr, mem::size_of};

use crate::{Errno, errno, sys_list_partitions, sys_mount};

/// Partition information returned by the kernel when listing partitions.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PartitionInfo {
    pub index: usize,
    pub starting_lba: u64,
    pub ending_lba: u64,
    pub size_sectors: u64,
    pub device_id: u64,
    pub unique_partition_guid: [u8; 16],
}

impl PartitionInfo {
    /// Returns the number of bytes occupied by a partition entry as written by the kernel.
    #[inline]
    pub const fn byte_size() -> usize {
        size_of::<PartitionInfo>()
    }
}

/// Mount a partition to a mount point directory.
pub fn mount_partition(
    device_id: u64,
    partition_idx: u64,
    mount_point: &CStr,
) -> Result<(), Errno> {
    let result = unsafe { sys_mount(device_id, partition_idx, mount_point.as_ptr().cast()) };
    if result == 0 { Ok(()) } else { Err(errno()) }
}

/// Retrieve all known partitions from the kernel.
pub fn list_partitions() -> Result<Vec<PartitionInfo>, Errno> {
    const INITIAL_CAPACITY: usize = 12;
    let entry_size = PartitionInfo::byte_size();

    let mut capacity = INITIAL_CAPACITY.max(1);
    loop {
        let mut buf: Vec<MaybeUninit<PartitionInfo>> = Vec::with_capacity(capacity);
        let buf_ptr = buf.as_mut_ptr() as *mut u8;
        let buf_size = capacity * entry_size;

        let written = unsafe { sys_list_partitions(buf_ptr, buf_size) };
        if written < 0 {
            return Err(errno());
        }

        let written = written as usize;
        if !written.is_multiple_of(entry_size) {
            return Err(Errno::EIO);
        }

        let count = written / entry_size;
        if count == capacity && written == buf_size {
            capacity = capacity.checked_mul(2).ok_or(Errno::ENOMEM)?;
            continue;
        }

        unsafe {
            buf.set_len(count);
            let mut buf = ManuallyDrop::new(buf);
            let ptr = buf.as_mut_ptr() as *mut PartitionInfo;
            let cap = buf.capacity();
            return Ok(Vec::from_raw_parts(ptr, count, cap));
        }
    }
}
