//! USB block I/O API: thin wrappers that send requests to the xHCI driver thread
//! via the USB_BLOCK_MAILBOX and wait for responses.
//!
//! These functions may be called from any kernel thread (e.g. FS worker threads).
//! The actual I/O is performed inside the xHCI driver thread which owns the
//! controller and transfer rings.

use alloc::vec::Vec;

use crate::{
    drivers::usb::xhci::{USB_BLOCK_MAILBOX, UsbBlockRequest, UsbBlockResponse, XhciError},
    fs::{Error as FsError, gpt::Partition},
    thread::scheduler::sched,
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
