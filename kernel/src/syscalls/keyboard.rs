use core::time::Duration;

use alloc::vec::Vec;
use pc_keyboard::DecodedKey;

use crate::{drivers::keyboard::KEYBOARD_BROADCAST, thread::scheduler::sched};

use super::Errno;

pub fn sys_keyboard_raw(timeout_milis: u64, scancodes_buffer: *mut u32, size: usize) -> i64 {
    if sched().current_thread_mut(|thread| {
        thread.errno = Errno::Clear;

        if scancodes_buffer.is_null() {
            thread.errno = Errno::EINVAL;
            return -1;
        }
        0
    }) != 0
    {
        return -1;
    }

    let mut buf = Vec::with_capacity(size);

    x86_64::instructions::interrupts::enable();

    let rx = KEYBOARD_BROADCAST.lock().subscribe_or_get();

    while buf.len() < size
        && let Some(key) = rx.try_recv()
    {
        buf.push(convert_key(key));
    }

    if buf.len() < size
        && let Ok(key) = rx.recv_timeout(Duration::from_millis(timeout_milis))
    {
        buf.push(convert_key(key));
    }

    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), scancodes_buffer, buf.len()) };

    buf.len() as i64
}

fn convert_key(key: DecodedKey) -> u32 {
    match key {
        DecodedKey::Unicode(c) => c as u32,
        DecodedKey::RawKey(scancode) => scancode as u32 | 0x80000000, // Set high bit for raw
    }
}
