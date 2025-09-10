use alloc::{
    string::{String, ToString},
    vec::Vec,
};
use core::{ffi::CStr, fmt};
use spin::Mutex;
use thiserror::Error;

use crate::{
    sys::{Errno, SYS_KERNEL_LOGS, SYS_RAW_INPUT, errno, syscall2, syscall3},
    sys_read, sys_write,
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
        }
    }
}

pub type IoResult<T> = Result<T, IoError>;

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

/// Try to get `count` key events. This may not fill up all the buffer, usually just returns one.
pub fn get_raw_input(timeout_ms: u64, out_buf: &mut Vec<KeyEvent>, count: usize) {
    let mut buf: Vec<u32> = alloc::vec![0; count];

    let result = unsafe {
        syscall3(
            SYS_RAW_INPUT,
            timeout_ms,
            buf.as_mut_ptr() as u64,
            count as u64,
        )
    };

    if result == !0u64 {
        return;
    }

    for scancode in buf {
        if scancode & 0x80000000 != 0 {
            // Raw scancode
            let key = KeyEvent::RawScancode((scancode & 0x7FFFFFFF) as u8);
            out_buf.push(key);
        } else {
            // Unicode character
            let key = KeyEvent::Unicode(char::from_u32(scancode).unwrap_or('\0'));
            out_buf.push(key);
        }
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
