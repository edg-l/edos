//! The session clipboard.
//!
//! Two kernel-owned buffers, so a copy survives the program that made it and
//! reaches one that shares nothing else with it. `kernel/src/window/clipboard.rs`
//! owns the storage and explains why there are two.

use crate::sys::{self, SYS_CLIPBOARD_GET, SYS_CLIPBOARD_SET};

/// Which buffer a call means.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Buffer {
    /// Filled by an explicit copy, read by an explicit paste.
    Clipboard = 0,
    /// Filled merely by selecting text, pasted with the middle mouse button.
    /// Kept apart from [`Buffer::Clipboard`] so selecting somewhere does not
    /// destroy what was deliberately copied.
    Primary = 1,
}

/// The most either buffer holds; a longer `set` is refused rather than
/// truncated. Mirrors `clipboard::MAX_LEN` in the kernel.
pub const MAX_LEN: usize = 64 * 1024;

/// Replace the contents of `buffer`.
pub fn set_bytes(buffer: Buffer, bytes: &[u8]) -> Result<(), i64> {
    let result = unsafe {
        sys::syscall3(
            SYS_CLIPBOARD_SET,
            buffer as u64,
            bytes.as_ptr() as u64,
            bytes.len() as u64,
        )
    };
    if sys::is_err(result) { Err(-1) } else { Ok(()) }
}

/// Replace the contents of `buffer` with text.
pub fn set(buffer: Buffer, text: &str) -> Result<(), i64> {
    set_bytes(buffer, text.as_bytes())
}

/// The contents of `buffer`.
///
/// Asks for the length first and then reads, so a buffer that grew between the
/// two calls is reported at whatever length the second call saw rather than
/// being copied past its end.
pub fn get_bytes(buffer: Buffer) -> Vec<u8> {
    let len = unsafe { sys::syscall3(SYS_CLIPBOARD_GET, buffer as u64, 0, 0) };
    if len as i64 <= 0 {
        return Vec::new();
    }

    let mut bytes = vec![0u8; len as usize];
    let copied = unsafe {
        sys::syscall3(
            SYS_CLIPBOARD_GET,
            buffer as u64,
            bytes.as_mut_ptr() as u64,
            bytes.len() as u64,
        )
    };
    if copied as i64 <= 0 {
        return Vec::new();
    }
    bytes.truncate((copied as usize).min(bytes.len()));
    bytes
}

/// The contents of `buffer` as text, with anything that is not UTF-8 dropped.
///
/// Lossy rather than failing, because a paste that silently does nothing is
/// harder to understand than one that pastes what it could.
pub fn get(buffer: Buffer) -> String {
    String::from_utf8_lossy(&get_bytes(buffer)).into_owned()
}
