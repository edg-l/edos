//! USB mass storage [`AsyncBlockDevice`] adapter.
//!
//! Wraps the existing xHCI `USB_BLOCK_MAILBOX`: each `submit_*` sends a
//! request and synchronously waits for the xHCI driver thread's reply, then
//! completes the handle. The mechanism is sync internally, but presents the
//! same async surface as AHCI so fs/* sees one trait, not two.

use alloc::{sync::Arc, vec};
use core::time::Duration;

use crate::thread::scheduler::thread_yield;
use crate::{
    drivers::{
        block_io::{self, AsyncBlockDevice, BlockBuffer, BlockError, BlockIoHandle, WriteFlags},
        usb::xhci::{USB_BLOCK_MAILBOX, UsbBlockRequest, UsbBlockResponse, XhciError},
    },
    interrupts::io::XHCI_DRIVER_THREAD_ID,
    thread::scheduler::{WakePriority, sched},
};

pub struct UsbBlockDevice {
    /// Capacity reported by READ CAPACITY during enumeration.
    block_count: u64,
}

fn xhci_to_block(_e: XhciError) -> BlockError {
    BlockError::Io
}

/// How long a filesystem thread waits for the xHCI driver thread to answer a
/// block request. A bulk transfer is bounded by `bulk_transfer`'s own poll
/// limit, so anything past this means the driver thread is wedged or never
/// picked the request up; failing the read beats wedging the caller forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

fn send_and_wait(req: UsbBlockRequest) -> Option<UsbBlockResponse> {
    // Spin-yield until the mailbox has been initialized by the xHCI driver
    // thread (mass-storage enumeration may not be done yet at first call).
    let mailbox = loop {
        if let Some(mb) = USB_BLOCK_MAILBOX.get() {
            break mb;
        }
        thread_yield();
    };
    let response = mailbox.send(req);
    if let Some(handle) = XHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread(handle, WakePriority::Normal);
    }
    let reply = response.wait_timeout(REQUEST_TIMEOUT);
    if reply.is_none() {
        crate::log!(
            "usb-msc: block request timed out after {}s",
            REQUEST_TIMEOUT.as_secs()
        );
    }
    reply
}

impl AsyncBlockDevice for UsbBlockDevice {
    fn submit_read(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let handle = BlockIoHandle::pending();
        let buf_len = buffer.len();
        let buf_ptr = buffer.as_mut_ptr();
        let result = if sectors == 0 || sectors > u16::MAX as u32 {
            Err(BlockError::InvalidArg)
        } else {
            let scratch = vec![0u8; buf_len];
            match send_and_wait(UsbBlockRequest::Read {
                lba,
                sectors: sectors as u16,
                buffer: scratch,
            }) {
                None => Err(BlockError::Timeout),
                Some(UsbBlockResponse::ReadResult(Ok(data))) => {
                    let n = data.len().min(buf_len);
                    // SAFETY: caller's contract — buffer outlives the handle.
                    unsafe {
                        core::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr, n);
                    }
                    Ok(())
                }
                Some(UsbBlockResponse::ReadResult(Err(e))) => Err(xhci_to_block(e)),
                Some(_) => unreachable!("USB driver replied with wrong response variant"),
            }
        };
        drop(buffer);
        handle.complete(result);
        Ok(handle)
    }

    fn submit_write(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
        _flags: WriteFlags,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let handle = BlockIoHandle::pending();
        let buf_len = buffer.len();
        let buf_ptr = buffer.as_ptr();
        let result = if sectors == 0 || sectors > u16::MAX as u32 {
            Err(BlockError::InvalidArg)
        } else {
            // SAFETY: caller pinned the buffer until handle terminal.
            let data = unsafe { core::slice::from_raw_parts(buf_ptr, buf_len).to_vec() };
            match send_and_wait(UsbBlockRequest::Write {
                lba,
                sectors: sectors as u16,
                data,
            }) {
                None => Err(BlockError::Timeout),
                Some(UsbBlockResponse::WriteResult(Ok(_))) => Ok(()),
                Some(UsbBlockResponse::WriteResult(Err(e))) => Err(xhci_to_block(e)),
                Some(_) => unreachable!("USB driver replied with wrong response variant"),
            }
        };
        drop(buffer);
        handle.complete(result);
        Ok(handle)
    }

    fn submit_flush(&self) -> Result<Arc<BlockIoHandle>, BlockError> {
        // USB mass storage in EDOS today doesn't expose a SCSI SYNCHRONIZE
        // CACHE command; the xHCI driver writes through to the device. A
        // flush is therefore a no-op at the trait level.
        let handle = BlockIoHandle::pending();
        handle.complete(Ok(()));
        Ok(handle)
    }

    fn sector_count(&self) -> u64 {
        self.block_count
    }
}

/// Register the singleton USB mass-storage device under `device_id` in the
/// kernel-wide block-io registry. Called from the xHCI driver thread once a
/// USB MSC device has been enumerated and the mailbox is ready.
pub fn register(device_id: u64, block_count: u64) {
    let dev: Arc<dyn AsyncBlockDevice> = Arc::new(UsbBlockDevice { block_count });
    block_io::register(device_id, dev);
}
