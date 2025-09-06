use core::time::Duration;

use alloc::sync::Arc;
use spin::RwLock;

use crate::{
    drivers::keyboard::KEYBOARD_BROADCAST,
    println,
    syscalls::Errno,
    thread::{
        broadcast::ReceiveError,
        pipe::{FileDescriptor, Pipe, StandardStream},
        scheduler::sched,
    },
};

pub fn sys_write(fd: u64, buffer_ptr: *const u8, count: usize) -> u64 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    if count == 0 {
        return 0;
    }

    let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, count) };

    match thread.fd_table.get_fd(fd) {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdout | StandardStream::Stderr => match core::str::from_utf8(buffer) {
                Ok(s) => {
                    println!("{}", s);
                    count as u64
                }
                Err(_) => {
                    println!(
                        "sys_write: Non-UTF8 data: {:02x?}",
                        &buffer[..count.min(64)]
                    );
                    count as u64
                }
            },
            StandardStream::Stdin => {
                thread.errno = Errno::EINVAL;
                !0u64
            }
        },
        Some(FileDescriptor::Pipe(pipe)) => {
            let mut pipe = pipe.write();
            pipe.buffer.extend_from_slice(buffer);
            count as u64
        }
        None => {
            thread.errno = Errno::EINVAL;
            !0u64
        }
    }
}

pub fn sys_pipe(pipe_fds: *mut [u64; 2]) -> i32 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    if pipe_fds.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    let pipe = Arc::new(RwLock::new(Pipe::new()));

    let read_fd = thread
        .fd_table
        .allocate_fd(FileDescriptor::Pipe(pipe.clone()));
    let write_fd = thread.fd_table.allocate_fd(FileDescriptor::Pipe(pipe));

    unsafe {
        (*pipe_fds)[0] = read_fd; // Read end
        (*pipe_fds)[1] = write_fd; // Write end
    }

    0
}

pub fn sys_close(fd: u64) -> i32 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    // Can't close standard streams
    if fd <= 2 {
        thread.errno = Errno::EINVAL;
        return -1;
    }

    match thread.fd_table.close_fd(fd) {
        Some(FileDescriptor::Pipe(pipe)) => {
            let mut pipe_guard = pipe.write();
            // TODO: dont assume
            pipe_guard.close_writer(); // Assume it was a write end for now
            0
        }
        Some(_) => 0, // Other FD types
        None => {
            thread.errno = Errno::EINVAL;
            -1
        }
    }
}

pub fn sys_read(fd: u64, buffer_ptr: *mut u8, count: usize) -> i64 {
    let sched = sched();
    let thread = sched.current_thread_mut();
    thread.errno = Errno::Clear;

    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    let buffer = unsafe { core::slice::from_raw_parts_mut(buffer_ptr, count) };

    match thread.fd_table.get_fd(fd) {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdin => {
                x86_64::instructions::interrupts::enable();
                read_from_stdin(buffer)
            }
            StandardStream::Stdout | StandardStream::Stderr => {
                thread.errno = Errno::EINVAL;
                -1
            }
        },
        Some(FileDescriptor::Pipe(pipe)) => {
            x86_64::instructions::interrupts::enable();
            read_from_pipe(pipe.clone(), buffer)
        }
        None => {
            thread.errno = Errno::EINVAL;
            -1
        }
    }
}

fn read_from_stdin(buffer: &mut [u8]) -> i64 {
    use pc_keyboard::DecodedKey;

    let rx = KEYBOARD_BROADCAST.subscribe();
    let mut bytes_read = 0;

    // Read until we get a newline or fill the buffer
    while bytes_read < buffer.len() {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(DecodedKey::Unicode('\n')) => {
                buffer[bytes_read] = b'\n';
                bytes_read += 1;
                break;
            }
            Ok(DecodedKey::Unicode('\r')) => {
                buffer[bytes_read] = b'\n';
                bytes_read += 1;
                break;
            }
            Ok(DecodedKey::Unicode(c)) if c.is_ascii() => {
                buffer[bytes_read] = c as u8;
                bytes_read += 1;
            }
            Ok(DecodedKey::Unicode('\u{8}')) => {
                // Backspace - remove last character if any
                bytes_read = bytes_read.saturating_sub(1);
            }
            Ok(_) => {
                // Ignore non-ASCII keys and raw keys
                continue;
            }
            Err(ReceiveError::Timeout) => {
                // Recv again
                continue;
            }
        }
    }

    KEYBOARD_BROADCAST.unsubscribe();

    bytes_read as i64
}

fn read_from_pipe(pipe: alloc::sync::Arc<spin::RwLock<Pipe>>, buffer: &mut [u8]) -> i64 {
    let mut pipe_guard = pipe.write();

    if pipe_guard.buffer.is_empty() && pipe_guard.closed {
        return 0; // EOF
    }

    let bytes_to_read = buffer.len().min(pipe_guard.buffer.len());
    if bytes_to_read == 0 {
        // Pipe is empty but not closed - for now return immediately (non-blocking)
        // TODO: Make this blocking later
        return 0;
    }

    // Copy data from pipe buffer
    buffer[..bytes_to_read].copy_from_slice(&pipe_guard.buffer[..bytes_to_read]);

    // Remove read data from pipe
    pipe_guard.buffer.drain(..bytes_to_read);

    bytes_to_read as i64
}
