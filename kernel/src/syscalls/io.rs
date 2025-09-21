use alloc::{string::ToString, vec::Vec};
use core::time::Duration;

use x86_64::instructions::interrupts;

use crate::fs::{FileKind, PollState, api as fs_api, path::Path};
use crate::log;
use crate::{
    drivers::{keyboard::KEYBOARD_BROADCAST, tty},
    syscalls::Errno,
    thread::{
        broadcast::ReceiveError,
        pipe::{FileDescriptor, FsFile, Pipe, StandardStream},
        scheduler::sched,
    },
    timer::Instant,
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
    let mut thread = info.lock();

    thread.errno = Errno::Clear;

    if count == 0 {
        return 0;
    }

    let buffer = unsafe { core::slice::from_raw_parts(buffer_ptr, count) }.to_vec();

    match thread.fd_table.get_fd(fd) {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdout | StandardStream::Stderr => {
                tty::write_output(&buffer);
                count as u64
            }
            StandardStream::Stdin => {
                thread.errno = Errno::EINVAL;
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
                        thread.errno = Errno::EINVAL;
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
                Err(_) => {
                    thread.errno = Errno::EINVAL;
                    Err(-1)
                }
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

    let path = match resolve_path(path_str, &thread.cwd) {
        Ok(path) => path,
        Err(_) => {
            thread.errno = Errno::EINVAL;
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

pub fn sys_list_dir(path_ptr: *const u8, buffer_ptr: *mut u8, buffer_size: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if path_ptr.is_null() || buffer_ptr.is_null() {
        thread.errno = Errno::EFAULT;
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

    let path = match resolve_path(path_str, &thread.cwd) {
        Ok(path) => path,
        Err(_) => {
            thread.errno = Errno::EINVAL;
            return -1;
        }
    };

    // Get directory listing via FS API
    interrupts::enable();
    let files = match fs_api::list_files(&path) {
        Ok(files) => files,
        Err(_) => {
            thread.errno = Errno::EINVAL;
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
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if events_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    let descriptor = match thread.fd_table.get_fd(fd).cloned() {
        Some(desc) => desc,
        None => {
            thread.errno = Errno::EBADF;
            return -1;
        }
    };

    let FileDescriptor::FsFile(file) = descriptor else {
        thread.errno = Errno::EINVAL;
        return -1;
    };

    interrupts::enable();

    match fs_api::poll(&file.path, Duration::from_millis(timeout_ms)) {
        Ok(state) => {
            unsafe {
                core::ptr::write(events_ptr, state);
            }
            0
        }
        Err(err) => {
            thread.errno = Errno::from(err);
            -1
        }
    }
}

pub fn sys_select(entries_ptr: *mut SelectFd, count: usize, timeout_ms: u64) -> i64 {
    if count == 0 {
        return 0;
    }

    let sched = sched();
    let info = sched.current_thread_info();

    if entries_ptr.is_null() {
        let mut thread = info.lock();
        thread.errno = Errno::EFAULT;
        return -1;
    }

    if count > 1024 {
        let mut thread = info.lock();
        thread.errno = Errno::EINVAL;
        return -1;
    }

    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    let mut entries: Vec<SelectFd> = Vec::with_capacity(count);
    for idx in 0..count {
        let entry = unsafe { core::ptr::read(entries_ptr.add(idx)) };
        entries.push(entry);
    }

    let mut descriptors = Vec::with_capacity(count);
    for entry in &mut entries {
        entry.result = PollState::none();
        let descriptor = match thread.fd_table.get_fd(entry.fd).cloned() {
            Some(desc) => desc,
            None => {
                thread.errno = Errno::EBADF;
                return -1;
            }
        };
        descriptors.push(descriptor);
    }

    drop(thread);

    let timeout = if timeout_ms == u64::MAX {
        None
    } else {
        log!("timeout for {:?}ms", timeout_ms);
        Some(Duration::from_millis(timeout_ms))
    };
    let start = Instant::now();

    interrupts::enable();

    loop {
        let mut ready = 0usize;

        for (idx, descriptor) in descriptors.iter().enumerate() {
            let interests = entries[idx].interests;
            if !interests.readable && !interests.writable && !interests.error {
                entries[idx].result = PollState::none();
                continue;
            }

            match poll_descriptor(descriptor, interests) {
                Ok(state) => {
                    entries[idx].result = state;
                    if (interests.readable && state.readable)
                        || (interests.writable && state.writable)
                        || state.error
                    {
                        ready += 1;
                    }
                }
                Err(errno) => {
                    let mut thread = info.lock();
                    thread.errno = errno;
                    return -1;
                }
            }
        }

        if ready > 0 {
            write_back_select_entries(entries_ptr, &entries);
            let mut thread = info.lock();
            thread.errno = Errno::Clear;
            return ready as i64;
        }

        let remaining = timeout.map(|target| target.saturating_sub(start.elapsed()));
        if let Some(rem) = remaining
            && rem.is_zero()
        {
            write_back_select_entries(entries_ptr, &entries);
            let mut thread = info.lock();
            thread.errno = Errno::Clear;
            return 0;
        }

        let wait_slice = remaining
            .filter(|rem| !rem.is_zero())
            .map(|rem| rem.min(Duration::from_millis(50)))
            .unwrap_or(Duration::from_millis(50));

        sched.thread_wait_timeout(wait_slice);
    }
}

fn write_back_select_entries(ptr: *mut SelectFd, entries: &[SelectFd]) {
    for (idx, entry) in entries.iter().enumerate() {
        unsafe {
            core::ptr::write(ptr.add(idx), *entry);
        }
    }
}

fn poll_descriptor(descriptor: &FileDescriptor, interests: PollState) -> Result<PollState, Errno> {
    let mut state = PollState::none();

    match descriptor {
        FileDescriptor::StandardStream(StandardStream::Stdin) => {
            if interests.readable {
                let rx = KEYBOARD_BROADCAST.lock().subscribe_or_get();
                if !rx.is_empty() {
                    state.readable = true;
                }
            }
            if interests.writable {
                state.writable = true;
            }
        }
        FileDescriptor::StandardStream(StandardStream::Stdout | StandardStream::Stderr) => {
            if interests.writable {
                state.writable = true;
            }
        }
        FileDescriptor::Pipe(pipe) => {
            let guard = pipe.read();
            if interests.readable
                && (!guard.buffer.is_empty() || (guard.closed && guard.writers == 0))
            {
                state.readable = true;
            }
            if interests.writable && !guard.closed && guard.writers > 0 {
                state.writable = true;
            }
            if interests.error && guard.closed && guard.writers == 0 {
                state.error = true;
            }
        }
        FileDescriptor::FsFile(file) => {
            let poll_state =
                fs_api::poll(&file.path, Duration::from_millis(0)).map_err(Errno::from)?;
            if interests.readable && poll_state.readable {
                state.readable = true;
            }
            if interests.writable && poll_state.writable {
                state.writable = true;
            }
            if poll_state.error {
                state.error = true;
            }
        }
    }

    Ok(state)
}

pub fn sys_getcwd(buffer_ptr: *mut u8, size: usize) -> i64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if buffer_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return -1;
    }

    if size == 0 {
        thread.errno = Errno::EINVAL;
        return -1;
    }

    // Get current working directory as string
    let cwd_str = thread.cwd.to_string();
    let cwd_bytes = cwd_str.as_bytes();

    // Need space for string + null terminator
    if cwd_bytes.len() + 1 > size {
        thread.errno = Errno::EINVAL;
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
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if path_ptr.is_null() {
        thread.errno = Errno::EFAULT;
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

    // Resolve the target path (absolute or relative to current cwd)
    let new_path = match resolve_path(path_str, &thread.cwd) {
        Ok(path) => path,
        Err(_) => {
            thread.errno = Errno::EINVAL;
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
                    thread.errno = Errno::EINVAL;
                    return -1;
                }
            }
            Err(_) => {
                thread.errno = Errno::EINVAL;
                return -1;
            }
        }
    }

    // Update the current working directory
    thread.cwd = new_path;
    0
}
