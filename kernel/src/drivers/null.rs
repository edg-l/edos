//! The three data-sink devices.
//!
//! `/dev/null` discards writes and reads as an empty file, `/dev/zero` reads as
//! an endless run of zero bytes, and `/dev/full` reads the same but fails every
//! write with `ENOSPC`, which is what makes it useful for testing that path.

use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

use crate::{
    fs::{
        DevFsDevice, DevFsError,
        handle::{Pollable, StaticPoll},
        register_device_str,
    },
    log,
};

pub fn init() {
    for (path, device) in [
        ("/null", Arc::new(NullDevice) as Arc<dyn DevFsDevice>),
        ("/zero", Arc::new(ZeroDevice)),
        ("/full", Arc::new(FullDevice)),
    ] {
        if let Err(err) = register_device_str(path, device) {
            log!("failed to register /dev{path}: {err:?}");
        }
    }
}

struct NullDevice;

impl DevFsDevice for NullDevice {
    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(StaticPoll::ready()))
    }

    fn read(&self, _offset: usize, _count: usize) -> Result<Vec<u8>, DevFsError> {
        Ok(Vec::new())
    }

    fn write(&self, _offset: usize, data: &[u8]) -> Result<usize, DevFsError> {
        Ok(data.len())
    }
}

struct ZeroDevice;

impl DevFsDevice for ZeroDevice {
    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(StaticPoll::ready()))
    }
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        Ok(vec![0u8; count])
    }

    fn write(&self, _offset: usize, data: &[u8]) -> Result<usize, DevFsError> {
        Ok(data.len())
    }
}

struct FullDevice;

impl DevFsDevice for FullDevice {
    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(StaticPoll::ready()))
    }
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        Ok(vec![0u8; count])
    }

    fn write(&self, _offset: usize, _data: &[u8]) -> Result<usize, DevFsError> {
        Err(DevFsError::NoSpace)
    }
}
