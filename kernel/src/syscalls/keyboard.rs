use core::time::Duration;

use pc_keyboard::DecodedKey;

use crate::drivers::keyboard::KEYBOARD_BROADCAST;

pub fn sys_keyboard_raw(timeout_milis: u64) -> i64 {
    x86_64::instructions::interrupts::enable();


    // TODO: improve this, maybe add oneshot option to broadcast.
    let rx = KEYBOARD_BROADCAST.subscribe();

    match rx.recv_timeout(Duration::from_millis(timeout_milis)) {
        Ok(key) => {
            KEYBOARD_BROADCAST.unsubscribe();
            // Convert DecodedKey to a simple u32 or struct
            match key {
                DecodedKey::Unicode(c) => c as u32 as i64,
                DecodedKey::RawKey(scancode) => (scancode as u32 | 0x80000000) as i64, // Set high bit for raw
            }
        }
        Err(_) => -1, // No key available
    }
}
