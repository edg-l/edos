use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::time::Duration;
use x86_64::instructions::interrupts::without_interrupts;

use spin::Mutex;

use crate::{
    fs::{DevFsDevice, DevFsError, PollState, handle::Pollable, register_device_str},
    thread::broadcast::Broadcaster,
};

const TTY_BUFFER_CAPACITY: usize = 16 * 1024;

static TTY_BUFFER: Mutex<VecDeque<u8>> = Mutex::new(VecDeque::new());
static TTY_NOTIFY: Broadcaster<()> = Broadcaster::new();

pub struct TtyDevice;

impl TtyDevice {
    pub fn register() {
        let device = Arc::new(Self);
        register_device_str("/tty0", device).expect("failed to register tty device");
    }
}

fn push_bytes(data: &[u8]) {
    if data.is_empty() {
        return;
    }

    let mut should_notify = false;
    {
        let mut buffer = TTY_BUFFER.lock();
        for &byte in data {
            match byte {
                b'\x08' | b'\x7f' => {
                    if buffer.pop_back().is_some() {
                        should_notify = true;
                    }
                }
                b'\r' => {
                    // Drop carriage returns; treat CRLF as a single newline.
                }
                value => {
                    if buffer.len() >= TTY_BUFFER_CAPACITY {
                        buffer.pop_front();
                    }
                    buffer.push_back(value);
                    should_notify = true;
                }
            }
        }
    }

    if should_notify {
        TTY_NOTIFY.broadcast(());
    }
}

pub fn write_output(data: &[u8]) {
    push_bytes(data);
}

pub fn init() {
    TtyDevice::register();
}

#[derive(Debug, Clone)]
struct TtyPoll;

impl Pollable for TtyPoll {
    fn poll(&self, timeout: Duration) -> PollState {
        {
            let buffer = TTY_BUFFER.lock();
            if !buffer.is_empty() {
                return PollState {
                    readable: true,
                    writable: true,
                    error: false,
                };
            }
        }

        let rx = TTY_NOTIFY.subscribe();
        rx.poll(timeout)
    }
}

impl DevFsDevice for TtyDevice {
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        let mut buffer = TTY_BUFFER.lock();

        while result.len() < count {
            match buffer.pop_front() {
                Some(byte) => result.push(byte),
                None => break,
            }
        }

        Ok(result)
    }

    fn write(&self, _offset: usize, data: &[u8]) -> Result<usize, DevFsError> {
        push_bytes(data);
        Ok(data.len())
    }

    fn poll(&self) -> Result<Box<dyn Pollable>, DevFsError> {
        Ok(Box::new(TtyPoll))
    }

    fn size(&self) -> u64 {
        TTY_BUFFER.lock().len() as u64
    }
}
