use alloc::{string::ToString, vec::Vec};
use core::time::Duration;

use x86_64::instructions::interrupts;

use crate::fs::{FileKind, PollState, api as fs_api, path::Path};
use crate::{
    drivers::{keyboard::KEYBOARD_BROADCAST, tty},
    syscalls::Errno,
    thread::{
        pipe::{FileDescriptor, FsFile, Pipe, StandardStream},
        scheduler::sched,
    },
};

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DirEntry {
    pub name_len: u32,     // Length of the filename
    pub file_type: u8,     // 0=File, 1=Directory, 2=Symlink, 3=Special, 4=device
    pub size: u64,         // File size in bytes
    pub attrs: u8,         // File attributes (readonly=1, hidden=2, system=4, archive=8)
    pub reserved: [u8; 2], // Padding for alignment
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SelectFd {
    pub fd: u64,
    pub interests: PollState,
    pub result: PollState,
}

fn file_kind_to_u8(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => 0,
        FileKind::Directory => 1,
        FileKind::Symlink => 2,
        FileKind::Special => 3,
    }
}

fn file_attrs_to_u8(attrs: crate::fs::FileAttrs) -> u8 {
    let mut result = 0u8;
    if attrs.readonly {
        result |= 1;
    }
    if attrs.hidden {
        result |= 2;
    }
    if attrs.system {
        result |= 4;
    }
    if attrs.archive {
        result |= 8;
    }
    result
}

pub(super) fn resolve_path(
    path_str: &str,
    cwd: &Path,
) -> Result<Path, crate::fs::path::ParseError> {
    if path_str.starts_with('/') {
        // Absolute path
        Path::parse(path_str).map(|p| p.normalize())
    } else {
        // Relative path - join with cwd
        let joined = cwd.join(path_str);
        Ok(joined.normalize())
    }
}

pub fn sys_write(fd: u64, buffer_ptr: *const u8, count: usize) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();

    info.lock().errno = Errno::Clear;

    if count == 0 {
        return 0;
    }

    let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, count) }.to_vec();

    match info.lock().fd_table.get_fd(fd).cloned() {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdout | StandardStream::Stderr => {
                tty::write_output(&buffer);
                count as u64
            }
            StandardStream::Stdin => {
                info.lock().errno = Errno::EINVAL;
                !0u64
            }
        },
        Some(FileDescriptor::Pipe(pipe)) => {
            // TODO: is it safe to get this lock here
            // let text = core::str::from_utf8(&buffer);
            // log!("Pipe: {:?}", text);
            let mut pipe = pipe.write();
            pipe.buffer.extend_from_slice(&buffer);
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
                        info.lock().errno = Errno::EINVAL;
                        return !0u64;
                    }
                }
            }
            match fs_api::write_bytes(&file.path, file.offset as usize, &buffer) {
                Ok(written) => {
                    let new_fd = FileDescriptor::FsFile(FsFile {
                        offset: file.offset + written,
                        ..file
                    });
                    info.lock().fd_table.replace_fd(fd, new_fd);
                    written
                }
                Err(_) => {
                    info.lock().errno = Errno::EINVAL;
                    !0u64
                }
            }
        }
        None => {
            info.lock().errno = Errno::EINVAL;
            !0u64
        }
    }
}

#[allow(unused)]
pub fn sys_close(fd: u64) -> i32 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    match info.lock().fd_table.close_fd(fd) {
        Some(FileDescriptor::Pipe(pipe)) => {
            let mut guard = pipe.write();
            guard.close_reader();
            0
        }
        Some(_) => 0,
        None => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

pub fn sys_read(fd: u64, buffer_ptr: *mut u8, count: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let fd_info = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.get_fd(fd).cloned()
    };

    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    interrupts::enable();

    // Track FsFile state so we can advance offsets without re-locking unnecessarily.
    let fs_state = match &fd_info {
        Some(FileDescriptor::FsFile(file)) => Some(file.clone()),
        _ => None,
    };

    // Get kernel data first - all potentially blocking operations happen here
    let kernel_data = match fd_info {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdin => read_from_stdin(count),
            StandardStream::Stdout | StandardStream::Stderr => {
                info.lock().errno = Errno::EINVAL;
                return -1;
            }
        },
        Some(FileDescriptor::Pipe(pipe)) => read_from_pipe(pipe.clone(), count),
        Some(FileDescriptor::FsFile(file)) => {
            match fs_api::read_bytes(&file.path, file.offset as usize, count) {
                Ok(data) => Ok(data),
                Err(_) => {
                    info.lock().errno = Errno::EINVAL;
                    Err(-1)
                }
            }
        }
        None => {
            info.lock().errno = Errno::EINVAL;
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
    if let Some(mut file) = fs_state {
        file.offset += bytes_to_copy as u64;
        info.lock()
            .fd_table
            .replace_fd(fd, FileDescriptor::FsFile(file));
    }

    bytes_to_copy as i64
}

fn read_from_stdin(max_count: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    use alloc::vec::Vec;
    use pc_keyboard::DecodedKey;

    let rx = KEYBOARD_BROADCAST.subscribe();
    let mut kernel_buffer = Vec::new();

    // Read until we get a newline or reach max count
    while kernel_buffer.len() < max_count {
        match rx.recv() {
            DecodedKey::Unicode('\n') => {
                kernel_buffer.push(b'\n');
                break;
            }
            DecodedKey::Unicode('\r') => {
                kernel_buffer.push(b'\n');
                break;
            }
            DecodedKey::Unicode(c) if c.is_ascii() => {
                kernel_buffer.push(c as u8);
            }
            DecodedKey::Unicode('\u{8}') => {
                // Backspace - remove last character if any
                kernel_buffer.pop();
            }
            _ => {
                // Ignore non-ASCII keys and raw keys
                continue;
            }
        }
    }

    KEYBOARD_BROADCAST.unsubscribe();

    Ok(kernel_buffer)
}

pub fn sys_open(path_ptr: *const u8, flags: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    // Copy C string from user memory (simple, bounded)
    let mut buf = Vec::new();
    for i in 0..1024usize {
        let c = unsafe { core::ptr::read_volatile(path_ptr.add(i)) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    // If no null terminator within bound, treat as invalid
    if buf.is_empty() || buf.len() == 1024 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let path_str = match core::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    let path = match resolve_path(path_str, &info.lock().cwd) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Determine initial offset and verify file exists; support create flag
    let append = (flags & 0x400) != 0; // O_APPEND
    let create = (flags & 0x40) != 0; // O_CREAT
    let mut offset = 0u64;
    interrupts::enable();
    match fs_api::file_info(&path) {
        Ok(info) => {
            if append {
                offset = info.size;
            }
        }
        Err(_) => {
            if create {
                if fs_api::create_file(&path).is_err() {
                    info.lock().errno = Errno::EINVAL;
                    return -1;
                }
            } else {
                info.lock().errno = Errno::EINVAL;
                return -1;
            }
        }
    }

    let desc = FileDescriptor::FsFile(FsFile {
        path,
        offset,
        append,
    });
    let fd = info.lock().fd_table.allocate_fd(desc);
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

pub fn sys_list_dir(path_ptr: *const u8, buffer_ptr: *mut u8, buffer_size: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() || buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    if buffer_size == 0 {
        return 0;
    }

    // Copy C string from user memory (simple, bounded)
    let mut buf = Vec::new();
    for i in 0..1024usize {
        let c = unsafe { core::ptr::read_volatile(path_ptr.add(i)) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    // If no null terminator within bound, treat as invalid
    if buf.is_empty() || buf.len() == 1024 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let path_str = match core::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    let path = match resolve_path(path_str, &info.lock().cwd) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Get directory listing via FS API
    interrupts::enable();
    let files = match fs_api::list_files(&path) {
        Ok(files) => files,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Serialize entries into user buffer
    let mut written = 0usize;
    let entry_size = core::mem::size_of::<DirEntry>();

    for file in &files {
        let name_bytes = file.name.as_bytes();
        let total_entry_size = entry_size + name_bytes.len();

        // Check if we have space for this entry
        if written + total_entry_size > buffer_size {
            break;
        }

        // Create DirEntry
        let entry = DirEntry {
            name_len: name_bytes.len() as u32,
            file_type: file_kind_to_u8(file.kind),
            size: file.size,
            attrs: file_attrs_to_u8(file.attrs),
            reserved: [0, 0],
        };

        // Copy DirEntry to user buffer
        let entry_bytes = unsafe {
            core::slice::from_raw_parts(&entry as *const DirEntry as *const u8, entry_size)
        };
        let user_entry_ptr = unsafe { buffer_ptr.add(written) };
        unsafe {
            core::ptr::copy_nonoverlapping(entry_bytes.as_ptr(), user_entry_ptr, entry_size);
        }
        written += entry_size;

        // Copy filename to user buffer
        let user_name_ptr = unsafe { buffer_ptr.add(written) };
        unsafe {
            core::ptr::copy_nonoverlapping(name_bytes.as_ptr(), user_name_ptr, name_bytes.len());
        }
        written += name_bytes.len();
    }

    written as i64
}

pub fn sys_poll(fd: u64, events_ptr: *mut PollState, timeout_ms: u64) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if events_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let descriptor = match info.lock().fd_table.get_fd(fd).cloned() {
        Some(desc) => desc,
        None => {
            info.lock().errno = Errno::EBADF;
            return -1;
        }
    };

    let FileDescriptor::FsFile(file) = descriptor else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };

    interrupts::enable();

    let timeout = Duration::from_millis(timeout_ms);

    match fs_api::poll(&file.path) {
        Ok(pollable) => {
            let state = pollable.poll(timeout);
            unsafe {
                core::ptr::write(events_ptr, state);
            }
            0
        }
        Err(err) => {
            info.lock().errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_getcwd(buffer_ptr: *mut u8, size: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    if size == 0 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    // Get current working directory as string
    let cwd_str = info.lock().cwd.to_string();
    let cwd_bytes = cwd_str.as_bytes();

    // Need space for string + null terminator
    if cwd_bytes.len() + 1 > size {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    // Copy the cwd string to user buffer
    unsafe {
        core::ptr::copy_nonoverlapping(cwd_bytes.as_ptr(), buffer_ptr, cwd_bytes.len());
        // Add null terminator
        core::ptr::write(buffer_ptr.add(cwd_bytes.len()), 0);
    }

    (cwd_bytes.len() + 1) as i64
}

pub fn sys_chdir(path_ptr: *const u8) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    // Copy C string from user memory (simple, bounded)
    let mut buf = Vec::new();
    for i in 0..1024usize {
        let c = unsafe { core::ptr::read_volatile(path_ptr.add(i)) };
        if c == 0 {
            break;
        }
        buf.push(c);
    }
    // If no null terminator within bound, treat as invalid
    if buf.is_empty() || buf.len() == 1024 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let path_str = match core::str::from_utf8(&buf) {
        Ok(s) => s,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Resolve the target path (absolute or relative to current cwd)
    let new_path = match resolve_path(path_str, &info.lock().cwd) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Verify the target exists and is a directory
    interrupts::enable();

    // Special case: root directory always exists and is always a directory
    if new_path.is_root() {
        // Root directory is always valid
    } else {
        match fs_api::file_info(&new_path) {
            Ok(file) => {
                if file.kind != crate::fs::FileKind::Directory {
                    info.lock().errno = Errno::EINVAL;
                    return -1;
                }
            }
            Err(_) => {
                info.lock().errno = Errno::EINVAL;
                return -1;
            }
        }
    }

    // Update the current working directory
    info.lock().cwd = new_path;
    0
}
