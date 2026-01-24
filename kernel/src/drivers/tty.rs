use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    fs::{
        DevFsDevice, DevFsError, PollState,
        handle::{PollEntry, PollKey, PollRegistration, Pollable},
        register_device_str,
    },
    thread::{broadcast::Broadcaster, mutex::BlockingMutex},
    util::uaccess::try_copy_from_user,
};

const TTY_BUFFER_CAPACITY: usize = 16 * 1024;

static TTY_BUFFER: BlockingMutex<VecDeque<u8>> = BlockingMutex::new(VecDeque::new());
static TTY_NOTIFY: Broadcaster<()> = Broadcaster::new();
static TTY_POLLERS: BlockingMutex<Vec<(PollKey, Arc<PollEntry>)>> = BlockingMutex::new(Vec::new());
static TTY_NEXT_POLL_KEY: AtomicU64 = AtomicU64::new(1);

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
    let mut resulting_state = None;
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
        if should_notify {
            resulting_state = Some(poll_state_for_len(buffer.len()));
        }
    }

    if should_notify {
        notify_pollers(resulting_state.expect("state must exist when notifying"));
        TTY_NOTIFY.broadcast(());
    }
}

fn poll_state_for_len(len: usize) -> PollState {
    PollState {
        readable: len > 0,
        writable: true,
        error: false,
        hangup: false,
        invalid: false,
    }
}

fn notify_pollers(state: PollState) {
    let pollers = TTY_POLLERS.lock();
    for (_, entry) in pollers.iter() {
        entry.update(state);
    }
}

/// Write directly from user space to TTY buffer.
/// Returns bytes written, or None on fault.
pub fn write_from_user(user_ptr: *const u8, len: usize) -> Option<usize> {
    const CHUNK_SIZE: usize = 256;

    if len == 0 {
        return Some(0);
    }

    let mut should_notify = false;
    let mut resulting_state = None;
    let mut total_processed = 0usize;

    {
        let mut buffer = TTY_BUFFER.lock();
        let mut chunk = [0u8; CHUNK_SIZE];

        while total_processed < len {
            let remaining = len - total_processed;
            let to_copy = remaining.min(CHUNK_SIZE);

            // Batch copy from user space to stack buffer
            if !unsafe {
                try_copy_from_user(chunk.as_mut_ptr(), user_ptr.add(total_processed), to_copy)
            } {
                // Fault - return what we processed so far
                if should_notify {
                    resulting_state = Some(poll_state_for_len(buffer.len()));
                }
                drop(buffer);
                if should_notify {
                    notify_pollers(resulting_state.unwrap());
                    TTY_NOTIFY.broadcast(());
                }
                return if total_processed > 0 {
                    Some(total_processed)
                } else {
                    None
                };
            }

            // Echo chunk to serial for debug visibility
            if let Ok(s) = core::str::from_utf8(&chunk[..to_copy]) {
                crate::serial::add_serial_log(s);
            }

            // Process the chunk
            for &byte in &chunk[..to_copy] {
                match byte {
                    b'\x08' | b'\x7f' => {
                        if buffer.pop_back().is_some() {
                            should_notify = true;
                        }
                    }
                    b'\r' => {}
                    value => {
                        if buffer.len() >= TTY_BUFFER_CAPACITY {
                            buffer.pop_front();
                        }
                        buffer.push_back(value);
                        should_notify = true;
                    }
                }
            }
            total_processed += to_copy;
        }

        if should_notify {
            resulting_state = Some(poll_state_for_len(buffer.len()));
        }
    }

    if should_notify {
        notify_pollers(resulting_state.unwrap());
        TTY_NOTIFY.broadcast(());
    }

    Some(len)
}

pub fn init() {
    TtyDevice::register();
}

#[derive(Debug, Clone)]
struct TtyPoll;

impl Pollable for TtyPoll {
    fn register(&self, entry: Arc<PollEntry>) -> PollRegistration {
        let state = {
            let buffer = TTY_BUFFER.lock();
            poll_state_for_len(buffer.len())
        };

        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = TTY_NEXT_POLL_KEY.fetch_add(1, Ordering::Relaxed);
            TTY_POLLERS.lock().push((key, entry));
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        TTY_POLLERS.lock().retain(|(stored, _)| *stored != key);
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
        let new_state = poll_state_for_len(buffer.len());
        drop(buffer);
        notify_pollers(new_state);

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
