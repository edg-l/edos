use alloc::{boxed::Box, collections::VecDeque, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    debug::lock_order::{RANK_DEVICE_POLLERS, RANK_TTY_BUFFER},
    fs::{
        DevFsDevice, DevFsError, PollState,
        handle::{PollKey, PollRef, PollRegistration, Pollable},
        register_device_str,
    },
    ranked_lock,
    thread::{broadcast::Broadcaster, mutex::BlockingMutex},
    util::uaccess::try_copy_from_user,
};

const TTY_BUFFER_CAPACITY: usize = 16 * 1024;

static TTY_BUFFER: BlockingMutex<VecDeque<u8>> = BlockingMutex::new(VecDeque::new());
static TTY_NOTIFY: Broadcaster<()> = Broadcaster::new();
static TTY_POLLERS: BlockingMutex<Vec<(PollKey, PollRef)>> = BlockingMutex::new(Vec::new());
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
    let mut notifications = None;
    {
        let mut buffer = ranked_lock!(RANK_TTY_BUFFER, "tty::push_bytes", TTY_BUFFER);
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
            notifications = Some(snapshot_pollers(poll_state_for_len(buffer.len())));
        }
    }

    if let Some(notifications) = notifications {
        notifications.flush();
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

/// Snapshot the pollers to notify, for the caller to flush once it has dropped
/// every lock.
///
/// Taken while the buffer lock is still held, which is what serializes it
/// against `TtyPoll::register`: a registration that read an empty buffer has
/// not yet joined the list, and would otherwise miss the wake for the bytes
/// that arrived in between.
fn snapshot_pollers(state: PollState) -> TtyNotifications {
    let pollers = ranked_lock!(RANK_DEVICE_POLLERS, "tty::snapshot_pollers", TTY_POLLERS);
    TtyNotifications {
        entries: pollers.iter().map(|(_, entry)| entry.clone()).collect(),
        state,
    }
}

/// Deferred poll notifications, flushed after releasing the TTY locks so a
/// wake never runs with them held.
struct TtyNotifications {
    entries: Vec<PollRef>,
    state: PollState,
}

impl TtyNotifications {
    fn flush(self) {
        for entry in &self.entries {
            entry.update(self.state);
        }
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
    let mut notifications = None;
    let mut total_processed = 0usize;
    let mut faulted = false;
    let mut chunk = [0u8; CHUNK_SIZE];

    // The buffer lock is taken per chunk rather than for the whole write, so it
    // is never live across the user copy: a copy can demand fault and park, and
    // a thread killed while parked never runs the guard's Drop, which would
    // leave every console writer blocked for good. The cost is that writes
    // longer than CHUNK_SIZE may interleave with another writer's; a TTY makes
    // no atomicity guarantee above that.
    while total_processed < len {
        let remaining = len - total_processed;
        let to_copy = remaining.min(CHUNK_SIZE);

        if !unsafe {
            try_copy_from_user(chunk.as_mut_ptr(), user_ptr.add(total_processed), to_copy)
        } {
            faulted = true;
            break;
        }

        // Echo chunk to serial for debug visibility
        if let Ok(s) = core::str::from_utf8(&chunk[..to_copy]) {
            crate::serial::add_serial_log(s);
        }

        {
            let mut buffer = ranked_lock!(RANK_TTY_BUFFER, "tty::write_from_user", TTY_BUFFER);
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
            if should_notify {
                notifications = Some(snapshot_pollers(poll_state_for_len(buffer.len())));
            }
        }
        total_processed += to_copy;
    }

    if let Some(notifications) = notifications {
        notifications.flush();
        TTY_NOTIFY.broadcast(());
    }

    if faulted && total_processed == 0 {
        return None;
    }
    Some(total_processed)
}

pub fn init() {
    TtyDevice::register();
}

#[derive(Debug, Clone)]
struct TtyPoll;

impl Pollable for TtyPoll {
    fn register(&self, entry: PollRef) -> PollRegistration {
        // The buffer lock is held across reading the state and joining the
        // poller list, so a write cannot land in the gap and notify a list this
        // entry has not reached yet.
        let buffer = ranked_lock!(RANK_TTY_BUFFER, "tty::poll_register", TTY_BUFFER);
        let state = poll_state_for_len(buffer.len());

        entry.update(state);

        if state.matches(entry.interests()) {
            PollRegistration {
                initial: state,
                key: None,
            }
        } else {
            let key = TTY_NEXT_POLL_KEY.fetch_add(1, Ordering::Relaxed);
            ranked_lock!(RANK_DEVICE_POLLERS, "tty::poll_register_list", TTY_POLLERS)
                .push((key, entry));
            PollRegistration {
                initial: state,
                key: Some(key),
            }
        }
    }

    fn unregister(&self, key: PollKey) {
        ranked_lock!(RANK_DEVICE_POLLERS, "tty::poll_unregister", TTY_POLLERS)
            .retain(|(stored, _)| *stored != key);
    }
}

impl DevFsDevice for TtyDevice {
    fn read(&self, _offset: usize, count: usize) -> Result<Vec<u8>, DevFsError> {
        if count == 0 {
            return Ok(Vec::new());
        }

        let mut result = Vec::new();
        let mut buffer = ranked_lock!(RANK_TTY_BUFFER, "tty::device_read", TTY_BUFFER);

        while result.len() < count {
            match buffer.pop_front() {
                Some(byte) => result.push(byte),
                None => break,
            }
        }
        let notifications = snapshot_pollers(poll_state_for_len(buffer.len()));
        drop(buffer);
        notifications.flush();

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
        ranked_lock!(RANK_TTY_BUFFER, "tty::device_size", TTY_BUFFER).len() as u64
    }
}

/// A `Pollable` for the console, for `stdin` reached as the standard stream
/// rather than through a descriptor of its own.
pub fn pollable() -> Box<dyn Pollable> {
    Box::new(TtyPoll)
}
