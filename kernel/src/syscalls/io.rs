use core::time::Duration;

use x86_64::instructions::interrupts;

use crate::fs::{api as fs_api, path::Path};
use crate::log;
use crate::{
    drivers::keyboard::KEYBOARD_BROADCAST,
    syscalls::Errno,
    thread::{
        broadcast::ReceiveError,
        pipe::{FileDescriptor, FsFile, Pipe, StandardStream},
        scheduler::sched,
    },
};

pub fn sys_write(fd: u64, buffer_ptr: *const u8, count: usize) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();

    thread.errno = Errno::Clear;

    if count == 0 {
        return 0;
    }

    let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, count) };

    match thread.fd_table.get_fd(fd) {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdout | StandardStream::Stderr => match core::str::from_utf8(buffer) {
                Ok(s) => {
                    // TODO: add stdout stream broadcast?
                    log!("{}", s);
                    count as u64
                }
                Err(_) => {
                    log!(
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
            // TODO: is it safe to get this lock here
            let mut pipe = pipe.write();
            pipe.buffer.extend_from_slice(buffer);
            count as u64
        }
        Some(FileDescriptor::FsFile(file)) => {
            // Write via FS API using current offset (append respected)
            let mut file = file.clone();
            interrupts::enable();
            if file.append {
                match fs_api::file_info(&file.path) {
                    Ok(info) => file.offset = info.size,
                    Err(_) => {
                        thread.errno = Errno::EINVAL;
                        return !0u64;
                    }
                }
            }
            match fs_api::write_bytes(&file.path, file.offset as usize, buffer) {
                Ok(written) => {
                    let new_fd = FileDescriptor::FsFile(FsFile { offset: file.offset + written, ..file });
                    thread.fd_table.replace_fd(fd, new_fd);
                    written
                }
                Err(_) => {
                    thread.errno = Errno::EINVAL;
                    !0u64
                }
            }
        }
        None => {
            thread.errno = Errno::EINVAL;
            !0u64
        }
    }
}

#[allow(unused)]
pub fn sys_close(fd: u64) -> i32 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    match thread.fd_table.close_fd(fd) {
        Some(FileDescriptor::Pipe(pipe)) => {
            let mut guard = pipe.write();
            guard.close_reader();
            0
        }
        Some(_) => 0,
        None => {
            thread.errno = Errno::EINVAL;
            -1
        }
    }
}

pub fn sys_read(fd: u64, buffer_ptr: *mut u8, count: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    // Get kernel data first - all potentially blocking operations happen here
    let kernel_data = match thread.fd_table.get_fd(fd) {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdin => {
                interrupts::enable();
                read_from_stdin(count)
            }
            StandardStream::Stdout | StandardStream::Stderr => {
                thread.errno = Errno::EINVAL;
                return -1;
            }
        },
        Some(FileDescriptor::Pipe(pipe)) => {
            interrupts::enable();
            read_from_pipe(pipe.clone(), count)
        }
        Some(FileDescriptor::FsFile(file)) => {
            let file = file.clone();
            interrupts::enable();
            match fs_api::read_bytes(&file.path, file.offset as usize, count) {
                Ok(data) => Ok(data),
                Err(_) => { thread.errno = Errno::EINVAL; Err(-1) }
            }
        }
        None => {
            thread.errno = Errno::EINVAL;
            return -1;
        }
    };

    // Handle kernel data result
    let data = match kernel_data {
        Ok(data) => data,
        Err(error_code) => return error_code,
    };

    let bytes_to_copy = data.len().min(count);
    if bytes_to_copy == 0 {
        return 0;
    }

    // Now do the atomic copy to user space - no context switches can happen here

    let user_buffer = unsafe { core::slice::from_raw_parts_mut(buffer_ptr, bytes_to_copy) };
    user_buffer.copy_from_slice(&data[..bytes_to_copy]);

    // Update file offset if reading from FsFile
    if let Some(FileDescriptor::FsFile(file)) = thread.fd_table.get_fd(fd).cloned() {
        let new_off = file.offset + bytes_to_copy as u64;
        thread.fd_table.replace_fd(
            fd,
            FileDescriptor::FsFile(FsFile {
                offset: new_off,
                ..file
            }),
        );
    }

    bytes_to_copy as i64
}

fn read_from_stdin(max_count: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    use alloc::vec::Vec;
    use pc_keyboard::DecodedKey;

    let rx = KEYBOARD_BROADCAST.lock().subscribe_or_get();
    let mut kernel_buffer = Vec::new();

    // Read until we get a newline or reach max count
    while kernel_buffer.len() < max_count {
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(DecodedKey::Unicode('\n')) => {
                kernel_buffer.push(b'\n');
                break;
            }
            Ok(DecodedKey::Unicode('\r')) => {
                kernel_buffer.push(b'\n');
                break;
            }
            Ok(DecodedKey::Unicode(c)) if c.is_ascii() => {
                kernel_buffer.push(c as u8);
            }
            Ok(DecodedKey::Unicode('\u{8}')) => {
                // Backspace - remove last character if any
                kernel_buffer.pop();
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

    KEYBOARD_BROADCAST.lock().unsubscribe();

    Ok(kernel_buffer)
}

pub fn sys_open(path_ptr: *const u8, flags: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if path_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    // Copy C string from user memory (simple, bounded)
    let mut buf = alloc::vec::Vec::new();
    for i in 0..1024usize {
        let c = unsafe { core::ptr::read_volatile(path_ptr.add(i)) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    // If no null terminator within bound, treat as invalid
    if buf.is_empty() || buf.len() == 1024 {
        thread.errno = Errno::EINVAL;
        return -1;
    }

    let path_str = match core::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => {
            thread.errno = Errno::EINVAL;
            return -1;
        }
    };

    let Ok(mut path) = Path::parse(path_str) else {
        thread.errno = Errno::EINVAL;
        return -1;
    };
    path = path.normalize();

    // Determine initial offset and verify file exists; support create flag
    let append = (flags & 0x400) != 0; // O_APPEND
    let create = (flags & 0x40) != 0; // O_CREAT
    let mut offset = 0u64;
    interrupts::enable();
    match fs_api::file_info(&path) {
        Ok(info) => {
            if append { offset = info.size; }
        }
        Err(_) => {
            if create {
                if let Err(_) = fs_api::create_file(&path) {
                    thread.errno = Errno::EINVAL;
                    return -1;
                }
            } else {
                thread.errno = Errno::EINVAL;
                return -1;
            }
        }
    }

    let desc = FileDescriptor::FsFile(FsFile {
        path,
        offset,
        append,
    });
    let fd = thread.fd_table.allocate_fd(desc);
    fd as i64
}

fn read_from_pipe(
    pipe: alloc::sync::Arc<spin::RwLock<Pipe>>,
    max_count: usize,
) -> Result<alloc::vec::Vec<u8>, i64> {
    use alloc::vec::Vec;

    let mut pipe_guard = pipe.write();

    if pipe_guard.buffer.is_empty() && pipe_guard.closed {
        return Ok(Vec::new()); // EOF
    }

    let bytes_to_read = max_count.min(pipe_guard.buffer.len());
    if bytes_to_read == 0 {
        // Pipe is empty but not closed - for now return immediately (non-blocking)
        // TODO: Make this blocking later
        return Ok(Vec::new());
    }

    // Copy data to kernel buffer
    let kernel_buffer = pipe_guard.buffer[..bytes_to_read].to_vec();

    // Remove read data from pipe
    pipe_guard.buffer.drain(..bytes_to_read);

    Ok(kernel_buffer)
}
