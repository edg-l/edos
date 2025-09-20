use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{ffi::CStr, fmt};
use spin::Mutex;
use thiserror::Error;

use crate::{
    sys::{Errno, SYS_IOCTL, SYS_KERNEL_LOGS, SYS_POLL, errno, syscall2, syscall3},
    sys_open as raw_sys_open, sys_read, sys_write,
};

/// I/O Error type with proper error handling
#[derive(Debug, Error, Clone, Copy)]
pub enum IoError {
    #[error("Invalid argument")]
    InvalidInput,
    #[error("Out of memory")]
    OutOfMemory,
    #[error("Bad address/fault")]
    Fault,
    #[error("Unknown error")]
    Unknown,
    #[error("Interrupted")]
    Interrupted,
}

impl From<Errno> for IoError {
    fn from(errno: Errno) -> Self {
        match errno {
            Errno::EINVAL => IoError::InvalidInput,
            Errno::ENOMEM => IoError::OutOfMemory,
            Errno::EFAULT => IoError::Fault,
            Errno::UNKNOWN => IoError::Unknown,
            Errno::Clear => IoError::Unknown, // Shouldn't happen but handle gracefully
            _ => IoError::Unknown,
        }
    }
}

pub type IoResult<T> = Result<T, IoError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File = 0,
    Directory = 1,
    Symlink = 2,
    Special = 3,
}

impl From<u8> for FileType {
    fn from(value: u8) -> Self {
        match value {
            0 => FileType::File,
            1 => FileType::Directory,
            2 => FileType::Symlink,
            _ => FileType::Special,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone)]
pub struct DirEntry {
    pub name: String,
    pub file_type: FileType,
    pub size: u64,
    pub attrs: u8,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct RawDirEntry {
    name_len: u32,
    file_type: u8,
    size: u64,
    attrs: u8,
    reserved: [u8; 2],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct PollState {
    pub readable: bool,
    pub writable: bool,
    pub error: bool,
}

/// Helper function to write all bytes to a file descriptor, handling partial writes
fn write_all_to_fd(fd: u64, mut buf: &[u8]) -> IoResult<()> {
    while !buf.is_empty() {
        let result = unsafe { sys_write(fd, buf.as_ptr(), buf.len()) };
        match result {
            n if n > 0 => {
                // Partial or complete write - advance buffer
                buf = &buf[n as usize..];
            }
            0 => {
                // Zero bytes written - this shouldn't normally happen for stdout/stderr
                // but treat as retriable error
                return Err(IoError::Interrupted);
            }
            _ => {
                // Negative result indicates an error
                let errno = errno();
                return Err(IoError::from(errno));
            }
        }
    }
    Ok(())
}

/// Thread-safe standard output
pub struct Stdout {
    _private: (),
}

impl Stdout {
    const fn new() -> Self {
        Self { _private: () }
    }

    /// Write all bytes to stdout, ensuring complete write or error
    pub fn write_all(&self, buf: &[u8]) -> IoResult<()> {
        write_all_to_fd(1, buf)
    }

    /// Write formatted arguments to stdout
    pub fn write_fmt(&self, args: fmt::Arguments<'_>) -> IoResult<()> {
        struct WriteAdapter<'a> {
            stdout: &'a Stdout,
            error: Option<IoError>,
        }

        impl<'a> fmt::Write for WriteAdapter<'a> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                match self.stdout.write_all(s.as_bytes()) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        self.error = Some(e);
                        Err(fmt::Error)
                    }
                }
            }
        }

        let mut adapter = WriteAdapter {
            stdout: self,
            error: None,
        };

        match fmt::write(&mut adapter, args) {
            Ok(()) => Ok(()),
            Err(_) => Err(adapter.error.unwrap_or(IoError::Unknown)),
        }
    }
}

/// Thread-safe standard error
pub struct Stderr {
    _private: (),
}

impl Stderr {
    const fn new() -> Self {
        Self { _private: () }
    }

    /// Write all bytes to stderr, ensuring complete write or error
    pub fn write_all(&self, buf: &[u8]) -> IoResult<()> {
        write_all_to_fd(2, buf)
    }

    /// Write formatted arguments to stderr
    pub fn write_fmt(&self, args: fmt::Arguments<'_>) -> IoResult<()> {
        struct WriteAdapter<'a> {
            stderr: &'a Stderr,
            error: Option<IoError>,
        }

        impl<'a> fmt::Write for WriteAdapter<'a> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                match self.stderr.write_all(s.as_bytes()) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        self.error = Some(e);
                        Err(fmt::Error)
                    }
                }
            }
        }

        let mut adapter = WriteAdapter {
            stderr: self,
            error: None,
        };

        match fmt::write(&mut adapter, args) {
            Ok(()) => Ok(()),
            Err(_) => Err(adapter.error.unwrap_or(IoError::Unknown)),
        }
    }
}

/// Global thread-safe stdout instance
pub static STDOUT: Mutex<Stdout> = Mutex::new(Stdout::new());

/// Global thread-safe stderr instance
pub static STDERR: Mutex<Stderr> = Mutex::new(Stderr::new());

/// Read from a file descriptor into buffer
/// Returns number of bytes read on success
pub fn read_from_fd(fd: u64, buf: &mut [u8]) -> IoResult<usize> {
    if buf.is_empty() {
        return Ok(0);
    }

    let result = unsafe { sys_read(fd, buf.as_mut_ptr(), buf.len()) };
    if result < 0 {
        let errno = errno();
        Err(IoError::from(errno))
    } else {
        Ok(result as usize)
    }
}

/// Read from stdin into buffer
/// Returns number of bytes read on success
pub fn read_stdin(buf: &mut [u8]) -> IoResult<usize> {
    read_from_fd(0, buf)
}

#[derive(Debug, Clone, Copy)]
pub enum KeyEvent {
    Unicode(char),
    RawScancode(u8),
}

static KEYBOARD_FD: Mutex<Option<u64>> = Mutex::new(None);

fn keyboard_fd() -> Option<u64> {
    let mut fd_guard = KEYBOARD_FD.lock();
    if let Some(fd) = *fd_guard {
        return Some(fd);
    }

    match open("/dev/kbd", 0) {
        Ok(fd) => {
            *fd_guard = Some(fd);
            Some(fd)
        }
        Err(_) => None,
    }
}

fn release_keyboard_fd(fd: u64) {
    let mut fd_guard = KEYBOARD_FD.lock();
    if fd_guard
        .as_ref()
        .map(|stored| *stored == fd)
        .unwrap_or(false)
        && let Some(fd) = fd_guard.take()
    {
        let _ = crate::sys_close(fd);
    }
}

fn read_keyboard_events(
    fd: u64,
    max_events: usize,
    out_buf: &mut Vec<KeyEvent>,
) -> IoResult<usize> {
    if max_events == 0 {
        return Ok(0);
    }

    let bytes_to_read = max_events
        .checked_mul(core::mem::size_of::<u32>())
        .ok_or(IoError::InvalidInput)?;

    let mut buffer = alloc::vec![0u8; bytes_to_read];
    let bytes_read = read_from_fd(fd, &mut buffer)?;

    if bytes_read == 0 {
        return Ok(0);
    }

    let mut appended = 0;
    for chunk in buffer[..bytes_read].chunks_exact(core::mem::size_of::<u32>()) {
        if appended >= max_events {
            break;
        }

        let value = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        out_buf.push(decode_key_event(value));
        appended += 1;
    }

    Ok(appended)
}

fn decode_key_event(value: u32) -> KeyEvent {
    if value & 0x8000_0000 != 0 {
        let scancode = (value & 0x7FFF_FFFF) as u8;
        KeyEvent::RawScancode(scancode)
    } else {
        let ch = char::from_u32(value).unwrap_or('\0');
        KeyEvent::Unicode(ch)
    }
}

/// Try to get `count` key events. This may not fill up all the buffer, usually just returns one.
pub fn get_raw_input(timeout_ms: u64, out_buf: &mut Vec<KeyEvent>, count: usize) {
    if count == 0 {
        return;
    }

    let Some(fd) = keyboard_fd() else {
        return;
    };

    let mut remaining = count;

    // Drain any pending events without blocking first.
    loop {
        match read_keyboard_events(fd, remaining, out_buf) {
            Ok(0) => break,
            Ok(read) => {
                remaining = remaining.saturating_sub(read);
                if remaining == 0 {
                    return;
                }
            }
            Err(err) => {
                if matches!(err, IoError::InvalidInput) {
                    // Likely overflow configuration; nothing we can do here.
                }
                release_keyboard_fd(fd);
                return;
            }
        }
    }

    if remaining == 0 || timeout_ms == 0 {
        return;
    }

    match poll_fd(fd, timeout_ms) {
        Ok(state) => {
            if !state.readable {
                return;
            }
        }
        Err(_) => {
            release_keyboard_fd(fd);
            return;
        }
    }

    if read_keyboard_events(fd, remaining, out_buf).is_err() {
        release_keyboard_fd(fd);
    }
}

pub fn get_kernel_logs() -> Vec<String> {
    let mut buf: Vec<u8> = alloc::vec![0; 1024];

    let result = unsafe { syscall2(SYS_KERNEL_LOGS, buf.as_mut_ptr() as u64, 1024_u64) };

    if result == !0u64 {
        return Vec::new();
    }

    let mut last_idx = 0;

    let mut logs = Vec::new();

    for (i, b) in buf.iter().enumerate() {
        if *b == 0 {
            let len = i - last_idx;

            if len > 0 {
                if let Ok(log) = CStr::from_bytes_with_nul(&buf[last_idx..(i + 1)]) {
                    logs.push(log.to_string_lossy().to_string());
                }
                last_idx = i + 1;
            }
        }

        if i >= result as usize {
            break;
        }
    }

    logs
}
/// Public helper to write all bytes to a file descriptor
pub fn write_all_fd(fd: u64, buf: &[u8]) -> IoResult<()> {
    write_all_to_fd(fd, buf)
}

/// Issue a device-specific ioctl on a file descriptor.
pub fn ioctl(fd: u64, request: u64, arg: u64) -> IoResult<u64> {
    let result = unsafe { syscall3(SYS_IOCTL, fd, request, arg) as isize };
    if result >= 0 {
        Ok(result as u64)
    } else {
        Err(IoError::from(errno()))
    }
}

/// Query poll state for a descriptor; `timeout_ms` currently advisory.
pub fn poll_fd(fd: u64, timeout_ms: u64) -> IoResult<PollState> {
    let mut state = PollState::default();
    let result = unsafe {
        syscall3(
            SYS_POLL,
            fd,
            (&mut state as *mut PollState) as u64,
            timeout_ms,
        ) as isize
    };

    if result >= 0 {
        Ok(state)
    } else {
        Err(IoError::from(errno()))
    }
}

/// File open flags
pub mod open_flags {
    /// Append writes to end of file
    pub const O_APPEND: u64 = 0x400;
    /// Create file if it does not exist
    pub const O_CREAT: u64 = 0x40;
}

/// Open a file by path with flags. Returns an fd.
pub fn open(path: &str, flags: u64) -> IoResult<u64> {
    // Build a C string (nul-terminated) in a scratch buffer
    let mut bytes = alloc::vec::Vec::with_capacity(path.len() + 1);
    bytes.extend_from_slice(path.as_bytes());
    bytes.push(0);

    let fd = unsafe { raw_sys_open(bytes.as_ptr(), flags) };
    if fd < 0 {
        Err(IoError::from(errno()))
    } else {
        Ok(fd as u64)
    }
}

/// Read fd until EOF (or up to max_bytes if provided Some)
pub fn read_to_end(fd: u64, max_bytes: Option<usize>) -> IoResult<Vec<u8>> {
    let mut out = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        let n = read_from_fd(fd, &mut buf)?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if let Some(max) = max_bytes
            && out.len() >= max
        {
            break;
        }
    }
    Ok(out)
}

/// List directory contents
pub fn list_dir(path: &str) -> IoResult<Vec<DirEntry>> {
    // Build a C string (nul-terminated) in a scratch buffer
    let mut path_bytes = alloc::vec::Vec::with_capacity(path.len() + 1);
    path_bytes.extend_from_slice(path.as_bytes());
    path_bytes.push(0);

    // Allocate buffer for directory entries (start with 4KB)
    let mut buffer = alloc::vec![0u8; 4096];

    let result =
        unsafe { crate::sys_list_dir(path_bytes.as_ptr(), buffer.as_mut_ptr(), buffer.len()) };

    if result < 0 {
        return Err(IoError::from(errno()));
    }

    let bytes_read = result as usize;
    if bytes_read == 0 {
        return Ok(Vec::new());
    }

    // Parse the serialized directory entries
    let mut entries = Vec::new();
    let mut offset = 0;

    while offset + core::mem::size_of::<RawDirEntry>() <= bytes_read {
        // Read the raw directory entry header
        let raw_entry =
            unsafe { core::ptr::read_unaligned(buffer.as_ptr().add(offset) as *const RawDirEntry) };
        offset += core::mem::size_of::<RawDirEntry>();

        // Ensure we have enough bytes for the filename
        if offset + raw_entry.name_len as usize > bytes_read {
            break; // Truncated entry, stop parsing
        }

        // Extract the filename
        let name_bytes = &buffer[offset..offset + raw_entry.name_len as usize];
        let name = match core::str::from_utf8(name_bytes) {
            Ok(s) => s.to_string(),
            Err(_) => continue, // Skip invalid UTF-8 filenames
        };
        offset += raw_entry.name_len as usize;

        // Create the directory entry
        let entry = DirEntry {
            name,
            file_type: FileType::from(raw_entry.file_type),
            size: raw_entry.size,
            attrs: raw_entry.attrs,
        };

        entries.push(entry);
    }

    Ok(entries)
}

/// Get current working directory
pub fn getcwd() -> IoResult<String> {
    // Start with reasonable buffer size
    let mut buffer = alloc::vec![0u8; 512];

    let result = unsafe { crate::sys_getcwd(buffer.as_mut_ptr(), buffer.len()) };

    if result < 0 {
        return Err(IoError::from(errno()));
    }

    let length = result as usize;
    if length == 0 {
        return Ok(String::new());
    }

    // Convert to string (should include null terminator)
    let cwd_bytes = &buffer[..length - 1]; // Exclude null terminator
    match core::str::from_utf8(cwd_bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(_) => Err(IoError::InvalidInput),
    }
}

/// Change current working directory
pub fn chdir(path: &str) -> IoResult<()> {
    // Build a C string (nul-terminated) in a scratch buffer
    let mut path_bytes = alloc::vec::Vec::with_capacity(path.len() + 1);
    path_bytes.extend_from_slice(path.as_bytes());
    path_bytes.push(0);

    let result = unsafe { crate::sys_chdir(path_bytes.as_ptr()) };

    if result < 0 {
        Err(IoError::from(errno()))
    } else {
        Ok(())
    }
}
