//! RAM-backed block device over the live root image Limine hands us.
//!
//! The backing bytes are the module's own memory, so writes go in place: the
//! live system is writable, costs no RAM beyond the image itself, and loses
//! everything on reboot. That is live-CD semantics, and it is deliberate.
//!
//! Every operation completes synchronously. Nothing here parks, allocates a
//! DMA buffer, or talks to hardware, so the returned handle is already in a
//! terminal state by the time the caller sees it.

use alloc::sync::Arc;

use x86_64::{PhysAddr, VirtAddr, structures::paging::PhysFrame};

use crate::{
    boot::boot_info,
    drivers::block_io::{
        self, AsyncBlockDevice, BlockBuffer, BlockError, BlockIoHandle, WriteFlags,
    },
    log,
    memory::frame_allocator::is_frame_reserved,
};

/// Block-io device id for the live root ramdisk. Above AHCI (0-based, one per
/// port) and USB mass storage (`1000 + idx`).
pub const RAMDISK_DEVICE_ID: u64 = 2000;

const SECTOR_SIZE: usize = 512;

pub struct RamBlockDevice {
    data: *mut u8,
    len: usize,
}

// SAFETY: the backing memory is owned exclusively by this device for the life
// of the boot; concurrent access is serialized by the block layer's callers
// the same way it is for a real disk.
unsafe impl Send for RamBlockDevice {}
// SAFETY: the same argument as the impl above.
unsafe impl Sync for RamBlockDevice {}

impl RamBlockDevice {
    /// Byte range of a request, or `InvalidArg` if it runs off the end.
    fn range(&self, lba: u64, sectors: u32, buf_len: usize) -> Result<(usize, usize), BlockError> {
        let count = sectors as usize * SECTOR_SIZE;
        if count == 0 || buf_len != count {
            return Err(BlockError::InvalidArg);
        }
        let start = (lba as usize)
            .checked_mul(SECTOR_SIZE)
            .ok_or(BlockError::InvalidArg)?;
        let end = start.checked_add(count).ok_or(BlockError::InvalidArg)?;
        if end > self.len {
            return Err(BlockError::InvalidArg);
        }
        Ok((start, count))
    }
}

impl AsyncBlockDevice for RamBlockDevice {
    fn submit_read(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let (start, count) = self.range(lba, sectors, buffer.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(self.data.add(start), buffer.as_mut_ptr(), count);
        }
        let handle = BlockIoHandle::pending();
        handle.complete(Ok(()));
        Ok(handle)
    }

    fn submit_write(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
        _flags: WriteFlags,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let (start, count) = self.range(lba, sectors, buffer.len())?;
        unsafe {
            core::ptr::copy_nonoverlapping(buffer.as_ptr(), self.data.add(start), count);
        }
        let handle = BlockIoHandle::pending();
        handle.complete(Ok(()));
        Ok(handle)
    }

    fn submit_flush(&self) -> Result<Arc<BlockIoHandle>, BlockError> {
        let handle = BlockIoHandle::pending();
        handle.complete(Ok(()));
        Ok(handle)
    }

    fn sector_count(&self) -> u64 {
        (self.len / SECTOR_SIZE) as u64
    }
}

/// Register the live root image as a block device, if the boot medium carried
/// one. Must run before `fs::init` scans for partitions.
pub fn init() {
    let Some(module) = boot_info().live_root else {
        return;
    };

    if module.len < SECTOR_SIZE {
        log!(
            "ramdisk: live root module is {} bytes, ignoring",
            module.len
        );
        return;
    }

    debug_assert!(
        module_frames_reserved(module.addr, module.len),
        "live root module sits in memory the frame allocator can hand out"
    );

    log!(
        "ramdisk: live root at {:p}, {} MiB, device {}",
        module.addr,
        module.len / (1024 * 1024),
        RAMDISK_DEVICE_ID
    );

    block_io::register(
        RAMDISK_DEVICE_ID,
        Arc::new(RamBlockDevice {
            data: module.addr,
            len: module.len,
        }) as Arc<dyn AsyncBlockDevice>,
    );
}

/// True when the module's first and last frame are marked allocated.
///
/// Limine places modules in bootloader-reclaimable memory, which the frame
/// allocator never frees: it starts with every frame allocated and releases
/// only `MEMMAP_USABLE` regions. Reclaimed root-image frames would look like
/// filesystem corruption rather than a memory bug, so assert the property
/// instead of relying on it silently.
fn module_frames_reserved(addr: *mut u8, len: usize) -> bool {
    let offset = boot_info().physical_memory_offset.as_u64();
    let base = VirtAddr::from_ptr(addr).as_u64();
    let frame_at = |virt: u64| PhysFrame::containing_address(PhysAddr::new(virt - offset));

    is_frame_reserved(frame_at(base)) && is_frame_reserved(frame_at(base + (len - 1) as u64))
}
