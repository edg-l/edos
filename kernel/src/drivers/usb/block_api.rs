//! USB block I/O API: thin wrappers that send requests to the xHCI driver thread
//! via the USB_BLOCK_MAILBOX and wait for responses.
//!
//! These functions may be called from any kernel thread (e.g. FS worker threads).
//! The actual I/O is performed inside the xHCI driver thread which owns the
//! controller and transfer rings.
//!
//! # Dead-thread / cancellation audit (Foundation #2, Phase 4)
//!
//! If the calling thread dies while parked in `response.wait()`, the following
//! happens:
//!
//! 1. `Response<UsbBlockResponse>` drops — its `Arc<ResponseInner>` refcount goes
//!    2 → 1. The second Arc is still held by `Request` in the mailbox queue.
//! 2. The xHCI driver eventually calls `req.reply(...)`, writes the value, sets
//!    `ready = true`, and calls `waitq.wake_one()`. Post-Foundation-#1 that wake
//!    is a no-op (no waiter enrolled on a dropped `Response`).
//! 3. `req.reply()` itself drops the `Request`, which drops the last
//!    `Arc<ResponseInner>`. `ResponseInner` deallocates cleanly.
//! 4. The `Vec<u8>` data buffer is owned by `Request.payload` (not by the
//!    caller's stack), so there is no use-after-free risk even if the calling
//!    thread's stack is gone.
//!
//! **No `CancellableOp` wrapper is needed.** The existing `Arc<ResponseInner>`
//! lifetime and Foundation-#1 silent-wake semantics are sufficient. The worst
//! outcome is one completed-but-discarded USB transfer — the data is read from
//! disk, placed in the `Vec`, and then freed when the `ResponseInner` drops.
//! No slot leak, no DMA-lifetime hazard, no waiter list corruption.

use alloc::vec::Vec;

use crate::{
    drivers::usb::xhci::{USB_BLOCK_MAILBOX, UsbBlockRequest, UsbBlockResponse, XhciError},
    fs::{Error as FsError, gpt::Partition},
    interrupts::io::XHCI_DRIVER_THREAD_ID,
    thread::scheduler::{WakePriority, sched},
};

/// Send a read request to the xHCI driver thread and wait for the result.
///
/// Returns the sector data as a `Vec<u8>`.
pub fn usb_read_sectors(lba: u64, sectors: u16, buffer: Vec<u8>) -> Result<Vec<u8>, XhciError> {
    let mailbox = loop {
        if let Some(mb) = USB_BLOCK_MAILBOX.get() {
            break mb;
        }
        sched().thread_yield();
    };

    let response = mailbox.send(UsbBlockRequest::Read {
        lba,
        sectors,
        buffer,
    });
    // Wake the xHCI driver thread so it processes the request.
    if let Some(handle) = XHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread(handle, WakePriority::Normal);
    }
    match response.wait() {
        UsbBlockResponse::ReadResult(result) => result,
        _ => unreachable!(),
    }
}

/// Send a write request to the xHCI driver thread and wait for the result.
///
/// Returns the data buffer on success so the caller can recycle it.
pub fn usb_write_sectors(lba: u64, sectors: u16, data: Vec<u8>) -> Result<Vec<u8>, XhciError> {
    let mailbox = loop {
        if let Some(mb) = USB_BLOCK_MAILBOX.get() {
            break mb;
        }
        sched().thread_yield();
    };

    let response = mailbox.send(UsbBlockRequest::Write { lba, sectors, data });
    if let Some(handle) = XHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread(handle, WakePriority::Normal);
    }
    match response.wait() {
        UsbBlockResponse::WriteResult(result) => result,
        _ => unreachable!(),
    }
}

/// Register a USB storage partition in the FS layer so it can be mounted.
///
/// Called by the xHCI driver thread after READ_CAPACITY succeeds.
pub fn register_usb_partition(partition: Partition) -> Result<(), FsError> {
    crate::fs::api::register_partition(partition)
}
