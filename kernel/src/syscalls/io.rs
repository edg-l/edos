use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::{string::ToString, vec::Vec};
use core::time::Duration;

use x86_64::instructions::interrupts;

use crate::fs::block_page_cache::BlockPageCache;
use crate::fs::handle::{PollEntry, PollKey, Pollable};
use crate::fs::vfs;
use crate::fs::{FileKind, PollState, api as fs_api, path::Path};
use crate::net::socket::PollableSocket;
use crate::thread::pipe::PollablePipe;
use crate::thread::poll::PollWaiter;
use crate::thread::pty::{PollablePtyMaster, PollablePtySlave};
use crate::thread::scheduler::{
    current_thread, current_thread_info, current_thread_weak, thread_exit, thread_park_while,
    thread_sleep,
};
use crate::util::uaccess::{try_copy_from_user, try_copy_to_user, try_write_user};
use crate::{
    drivers::{keyboard::KEY_EVENT_BROADCAST, random, tty},
    log,
    syscalls::{Errno, MAX_PATH_LEN, PathBuf, copy_user_path},
    thread::{
        UserThreadInfo,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        pipe::{FileDescriptor, FsFile, OpenMode, StandardStream},
        pty::Pty,
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

struct PollContext {
    index: usize,
    interests: PollState,
    pollable: Box<dyn Pollable>,
    entry: Arc<PollEntry>,
    key: Option<PollKey>,
}

const MAX_RANDOM_LEN: usize = 1 << 20;

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

/// The calling thread's working directory.
///
/// The `cwd` mutex is taken only after the per-thread `IrqSpinlock` guard is
/// gone. Writing this as one expression keeps that guard alive to the end of the
/// statement, so the `BlockingMutex` would be acquired with interrupts disabled,
/// and a contended acquisition there parks with them off.
pub(super) fn current_cwd(info: &Arc<IrqSpinlock<UserThreadInfo>>) -> Path {
    let cwd = info.lock().cwd.clone();
    let path = cwd.lock().clone();
    path
}

/// Replace the calling thread's working directory, taking the locks in the
/// order [`current_cwd`] documents.
pub(super) fn set_current_cwd(info: &Arc<IrqSpinlock<UserThreadInfo>>, path: Path) {
    let cwd = info.lock().cwd.clone();
    *cwd.lock() = path;
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
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    if count == 0 {
        return 0;
    }
    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    interrupts::enable();

    let fdinfo = fd_table.lock().get_fd(fd).cloned();

    match fdinfo {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdout | StandardStream::Stderr => {
                match tty::write_from_user(buffer_ptr, count) {
                    Some(n) => n as u64,
                    None => {
                        info.lock().errno = Errno::EFAULT;
                        !0u64
                    }
                }
            }
            StandardStream::Stdin => {
                info.lock().errno = Errno::EINVAL;
                !0u64
            }
        },
        Some(FileDescriptor::PipeWrite(pipe)) => {
            // Direct copy from user to pipe, flush notifications after dropping lock.
            let (result, notif) = {
                let mut pipe = pipe.lock();
                pipe.write_from_user(buffer_ptr, count)
            };
            notif.flush();
            match result {
                Some(n) => n as u64,
                None => {
                    info.lock().errno = Errno::EFAULT;
                    !0u64
                }
            }
        }
        Some(FileDescriptor::PipeRead(_)) => {
            info.lock().errno = Errno::EINVAL;
            !0u64
        }
        Some(FileDescriptor::PtyMaster(pty)) => {
            let (result, notif) = {
                let mut guard = pty.lock();
                guard.master_write_from_user(buffer_ptr, count)
            };
            notif.flush();
            match result {
                Some(n) => n as u64,
                None => {
                    info.lock().errno = Errno::EFAULT;
                    !0u64
                }
            }
        }
        Some(FileDescriptor::PtySlave(pty)) => {
            let (result, notif) = {
                let mut guard = pty.lock();
                guard.slave_write_from_user(buffer_ptr, count)
            };
            notif.flush();
            match result {
                Some(n) => n as u64,
                None => {
                    info.lock().errno = Errno::EFAULT;
                    !0u64
                }
            }
        }
        Some(FileDescriptor::FsFile(file)) => {
            const MAX_WRITE_SIZE: usize = 1024 * 1024; // 1 MiB
            let capped_count = count.min(MAX_WRITE_SIZE);

            let fs = match file.fs.as_ref() {
                Some(f) => f,
                None => {
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            };
            let op = vfs::VfsOp {
                fs: fs.clone(),
                // Invariant: relative is Some iff fs is Some (set together at open time).
                relative: file.relative.clone().expect("fs set without relative path"),
                inode: file.inode.clone(),
                mount_id: file.mount_id,
            };

            match vfs::write_from_user(
                &op,
                file.offset as usize,
                buffer_ptr,
                capped_count,
                file.append,
            ) {
                Ok(written) => {
                    let new_fd = FileDescriptor::FsFile(FsFile {
                        offset: file.offset + written,
                        ..file
                    });
                    fd_table.lock().replace_fd(fd, new_fd);
                    written
                }
                Err(_) => {
                    info.lock().errno = Errno::EINVAL;
                    !0u64
                }
            }
        }
        Some(FileDescriptor::Socket(sock)) => {
            use crate::net::socket::SOCK_STREAM;
            use crate::net::tcp::TcpState;
            use crate::net::{ipv4, stack::net_stack};

            const MAX_SOCKET_WRITE: usize = 1024 * 1024; // 1 MiB
            let count = count.min(MAX_SOCKET_WRITE);

            let sock_type = sock.lock().sock_type;
            if sock_type == SOCK_STREAM {
                // TCP write: segment data and send
                let conn = match sock.lock().tcp_conn.clone() {
                    Some(c) => c,
                    None => {
                        info.lock().errno = Errno::ENOTCONN;
                        return !0u64;
                    }
                };

                // Check connection state before writing
                let state = conn.lock().state;
                match state {
                    TcpState::Established | TcpState::CloseWait => {}
                    _ => {
                        info.lock().errno = Errno::EPIPE;
                        return !0u64;
                    }
                }

                let mut data = vec![0u8; count];
                if !unsafe { try_copy_from_user(data.as_mut_ptr(), buffer_ptr, count) } {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }

                let (segments, bytes_sent) = conn.lock().build_data_segments(&data);
                if segments.is_empty() {
                    // Window full: return 0 so caller can retry
                    return 0;
                }

                let remote_ip = conn.lock().remote_ip;
                let mut stack = net_stack().lock();
                for seg in &segments {
                    let _ = stack.send_ip(remote_ip, ipv4::IpProtocol::Tcp, seg);
                }
                bytes_sent as u64
            } else {
                // UDP write: send to stored remote_addr
                let remote = sock.lock().remote_addr;
                match remote {
                    Some(dst) => {
                        let mut data = vec![0u8; count];
                        if !unsafe { try_copy_from_user(data.as_mut_ptr(), buffer_ptr, count) } {
                            info.lock().errno = Errno::EFAULT;
                            return !0u64;
                        }
                        let src_port = sock.lock().local_addr.map(|a| a.port).unwrap_or(0);
                        match net_stack()
                            .lock()
                            .send_udp(src_port, dst.ip, dst.port, &data)
                        {
                            Ok(()) => count as u64,
                            Err(_) => {
                                info.lock().errno = Errno::EIO;
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
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    interrupts::enable();
    let result = fd_table.lock().close_fd(fd);
    match result {
        Some(FileDescriptor::PipeRead(pipe)) => {
            let notif = {
                let mut guard = pipe.lock();
                guard.close_reader()
            };
            notif.flush();
            0
        }
        Some(FileDescriptor::PipeWrite(pipe)) => {
            let notif = {
                let mut guard = pipe.lock();
                guard.close_writer()
            };
            notif.flush();
            0
        }
        Some(FileDescriptor::PtyMaster(pty)) => {
            let notif = pty.lock().close_master();
            notif.flush();
            0
        }
        Some(FileDescriptor::PtySlave(pty)) => {
            let notif = pty.lock().close_slave();
            notif.flush();
            0
        }
        Some(FileDescriptor::Socket(sock)) => {
            let mut s = sock.lock();
            s.refcount = s.refcount.saturating_sub(1);
            if s.refcount > 0 {
                return 0;
            }
            s.closed = true;
            let notif = s.notify_pollers();
            s.rx_wq.wake_all();
            if let Some(addr) = s.local_addr {
                let proto = if s.sock_type == crate::net::socket::SOCK_DGRAM {
                    17u8
                } else {
                    6u8
                };
                crate::net::socket::port_table()
                    .lock()
                    .remove(&(proto, addr.port));
            }
            let tcp_conn = s.tcp_conn.clone();
            drop(s);
            notif.flush();
            // For TCP sockets, send FIN to initiate graceful close
            if let Some(conn) = tcp_conn {
                let fin = conn.lock().build_fin();
                if let Some(fin_seg) = fin {
                    let remote_ip = conn.lock().remote_ip;
                    if let Some(stack_mutex) = crate::net::stack::NET_STACK.get() {
                        let mut stack = stack_mutex.lock();
                        let _ =
                            stack.send_ip(remote_ip, crate::net::ipv4::IpProtocol::Tcp, &fin_seg);
                    }
                }
            }
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
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    // The fd table is a BlockingMutex, so it must not be acquired under the
    // UserThreadInfo IrqSpinlock: threads of one process share the table, and a
    // contended acquisition with interrupts off spins without answering IPIs.
    interrupts::enable();
    let fd_info = fd_table.lock().get_fd(fd).cloned();

    match fd_info {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdin => {
                // Stdin reads from keyboard - still needs intermediate buffer
                let kernel_data = match read_from_stdin(count) {
                    Ok(data) => data,
                    Err(code) => return code,
                };
                let bytes_to_copy = kernel_data.len().min(count);
                if bytes_to_copy == 0 {
                    return 0;
                }
                if !unsafe { try_copy_to_user(buffer_ptr, kernel_data.as_ptr(), bytes_to_copy) } {
                    info.lock().errno = Errno::EFAULT;
                    return -1;
                }
                bytes_to_copy as i64
            }
            StandardStream::Stdout | StandardStream::Stderr => {
                info.lock().errno = Errno::EINVAL;
                -1
            }
        },
        Some(FileDescriptor::PipeRead(pipe)) => {
            let reader_wq = pipe.lock().reader_wq.clone();
            // Block until data is available or all writers are closed (EOF).
            loop {
                let (result, closed, notif) = {
                    let mut guard = pipe.lock();
                    let (r, n) = guard.read_to_user(buffer_ptr, count);
                    (r, guard.closed && guard.buffer.is_empty(), n)
                };
                notif.flush();

                match result {
                    Some(n) if n > 0 => break n as i64,
                    Some(_) if closed => break 0, // EOF: no data and all writers closed
                    Some(_) => {
                        // No data but writer still open: park until woken by write/close
                        reader_wq.wait_until(|| {
                            let guard = pipe.lock();
                            !guard.buffer.is_empty() || guard.closed
                        });
                        continue;
                    }
                    None => {
                        info.lock().errno = Errno::EFAULT;
                        break -1;
                    }
                }
            }
        }
        Some(FileDescriptor::PipeWrite(_)) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
        Some(FileDescriptor::PtyMaster(pty)) => {
            // Master reads are non-blocking: return 0 immediately when no data
            // is available. The master side is typically used in a poll loop
            // (e.g. the terminal emulator) and must not block, or it cannot
            // forward keyboard input to the slave -- causing a deadlock.
            let (result, eof, notif) = {
                let mut guard = pty.lock();
                let (r, n) = guard.master_read_to_user(buffer_ptr, count);
                let eof = guard.closed_slave && guard.output_buf.is_empty();
                (r, eof, n)
            };
            notif.flush();

            match result {
                Some(n) if n > 0 => n as i64,
                Some(_) if eof => 0,
                Some(_) => 0, // No data yet, return immediately
                None => {
                    info.lock().errno = Errno::EFAULT;
                    -1
                }
            }
        }
        Some(FileDescriptor::PtySlave(pty)) => {
            // Clone the input_wq Arc before entering the loop (avoids holding lock while blocking).
            let input_wq = pty.lock().input_wq();
            loop {
                // If this thread has been killed (e.g. Ctrl+C), force-exit
                // immediately. We can't rely on userspace to handle EINTR
                // because Rust std's read_to_string retries EINTR in a loop.
                let is_killed = current_thread().map_or(false, |t| {
                    t.killed.load(core::sync::atomic::Ordering::Acquire)
                });
                if is_killed {
                    thread_exit(130); // 128 + SIGINT(2)
                }

                let (result, eof, notif) = {
                    let mut guard = pty.lock();
                    let (r, n) = guard.slave_read_to_user(buffer_ptr, count);
                    let eof = guard.closed_master && guard.input_buf.is_empty();
                    (r, eof, n)
                };
                notif.flush();

                match result {
                    Some(n) if n > 0 => break n as i64,
                    Some(_) if eof => break 0,
                    Some(_) => {
                        input_wq.wait_until(|| {
                            let guard = pty.lock();
                            let killed = current_thread().map_or(false, |t| {
                                t.killed.load(core::sync::atomic::Ordering::Acquire)
                            });
                            !guard.input_buf.is_empty() || guard.closed_master || killed
                        });
                        continue;
                    }
                    None => {
                        info.lock().errno = Errno::EFAULT;
                        break -1;
                    }
                }
            }
        }
        Some(FileDescriptor::FsFile(file)) => {
            // Snapshot readahead state before the devfs/vfs branch split.
            let mut ra = file.ra;
            let offset = file.offset as usize;

            // Fast path: devfs devices can be read directly without the FS Mailbox.
            let (bytes_read, ra) = if let Some(device) =
                crate::fs::devfs::try_lookup_from_full_path(&file.path)
            {
                match device.read(offset, count) {
                    Ok(data) => {
                        let bytes_to_copy = data.len().min(count);
                        if bytes_to_copy == 0 {
                            return 0;
                        }
                        if !unsafe { try_copy_to_user(buffer_ptr, data.as_ptr(), bytes_to_copy) } {
                            info.lock().errno = Errno::EFAULT;
                            return -1;
                        }
                        (bytes_to_copy, ra) // devfs doesn't mutate ra
                    }
                    Err(_) => {
                        info.lock().errno = Errno::EIO;
                        return -1;
                    }
                }
            } else {
                let fs = match file.fs.as_ref() {
                    Some(f) => f,
                    None => {
                        info.lock().errno = Errno::EINVAL;
                        return -1;
                    }
                };
                let op = vfs::VfsOp {
                    fs: fs.clone(),
                    // Invariant: relative is Some iff fs is Some (set together at open time).
                    relative: file.relative.clone().expect("fs set without relative path"),
                    inode: file.inode.clone(),
                    mount_id: file.mount_id,
                };
                match vfs::read_to_user(&op, &mut ra, offset, count, buffer_ptr) {
                    Ok(n) => (n, ra),
                    Err(_) => {
                        info.lock().errno = Errno::EINVAL;
                        return -1;
                    }
                }
            };

            let new_fd = FileDescriptor::FsFile(FsFile {
                offset: file.offset + bytes_read as u64,
                ra,
                ..file
            });
            fd_table.lock().replace_fd(fd, new_fd);
            bytes_read as i64
        }
        Some(FileDescriptor::Socket(sock)) => {
            use crate::net::socket::SOCK_STREAM;
            use crate::net::tcp::TcpState;

            let sock_type = sock.lock().sock_type;
            if sock_type == SOCK_STREAM {
                // TCP read: drain from TcpConnection rx_buffer
                let conn = match sock.lock().tcp_conn.clone() {
                    Some(c) => c,
                    None => {
                        info.lock().errno = Errno::ENOTCONN;
                        return -1;
                    }
                };
                // `wait_until` parks once and returns; it does not guarantee the
                // condition holds, because a wake token left by an earlier wait
                // aborts the park. Loop on the real condition, or a read
                // reports EOF the moment anything else has woken this thread —
                // which is what made every TCP read return 0 bytes.
                let rx_wq = conn.lock().rx_wq.clone();
                let mut c = loop {
                    let ready = || {
                        let c = conn.lock();
                        !c.rx_buffer.is_empty()
                            || c.state == TcpState::Closed
                            || c.state == TcpState::CloseWait
                            || c.state == TcpState::TimeWait
                    };
                    if ready() {
                        break conn.lock();
                    }
                    rx_wq.wait_until(ready);
                };

                if c.rx_buffer.is_empty() {
                    return 0; // EOF
                }
                let bytes_to_read = count.min(c.rx_buffer.len());
                let data: Vec<u8> = c.rx_buffer.drain(..bytes_to_read).collect();
                drop(c);

                if !unsafe { try_copy_to_user(buffer_ptr, data.as_ptr(), bytes_to_read) } {
                    info.lock().errno = Errno::EFAULT;
                    return -1;
                }
                bytes_to_read as i64
            } else {
                // UDP: blocking receive from rx_queue
                // Same contract as the TCP path above: loop on the condition.
                let rx_wq = sock.lock().rx_wq.clone();
                loop {
                    let ready = || {
                        let s = sock.lock();
                        !s.rx_queue.is_empty() || s.closed
                    };
                    if ready() {
                        break;
                    }
                    rx_wq.wait_until(ready);
                }
                let data_opt = {
                    let mut s = sock.lock();
                    if s.closed && s.rx_queue.is_empty() {
                        return 0;
                    }
                    s.rx_queue.pop_front().map(|(d, _src)| d)
                };
                match data_opt {
                    Some(data) => {
                        let bytes_to_copy = data.len().min(count);
                        if !unsafe { try_copy_to_user(buffer_ptr, data.as_ptr(), bytes_to_copy) } {
                            info.lock().errno = Errno::EFAULT;
                            return -1;
                        }
                        bytes_to_copy as i64
                    }
                    None => 0,
                }
            }
        }
        None => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

pub fn sys_getrandom(buffer_ptr: *mut u8, count: usize, flags: u64) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    if count == 0 {
        return 0;
    }

    if count > MAX_RANDOM_LEN || flags != 0 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let mut kernel_buffer = vec![0u8; count];
    random::fill_bytes(&mut kernel_buffer);

    if !unsafe { try_copy_to_user(buffer_ptr, kernel_buffer.as_ptr(), kernel_buffer.len()) } {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    kernel_buffer.len() as i64
}

fn read_from_stdin(max_count: usize) -> Result<alloc::vec::Vec<u8>, i64> {
    use alloc::vec::Vec;
    use pc_keyboard::{KeyCode, KeyState};

    let rx = KEY_EVENT_BROADCAST.subscribe();
    let mut kernel_buffer = Vec::new();

    // Simple keycode→ASCII for raw stdin (no layout, no modifiers).
    // This is a fallback path; the terminal handles real keyboard input.
    while kernel_buffer.len() < max_count {
        let event = rx.recv();
        if event.state != KeyState::Down {
            continue;
        }
        match event.code {
            KeyCode::Return | KeyCode::NumpadEnter => {
                kernel_buffer.push(b'\n');
                break;
            }
            KeyCode::Backspace => {
                kernel_buffer.pop();
            }
            KeyCode::Spacebar => kernel_buffer.push(b' '),
            code => {
                // Basic letter/digit mapping (lowercase only)
                let ch = match code {
                    KeyCode::A => b'a',
                    KeyCode::B => b'b',
                    KeyCode::C => b'c',
                    KeyCode::D => b'd',
                    KeyCode::E => b'e',
                    KeyCode::F => b'f',
                    KeyCode::G => b'g',
                    KeyCode::H => b'h',
                    KeyCode::I => b'i',
                    KeyCode::J => b'j',
                    KeyCode::K => b'k',
                    KeyCode::L => b'l',
                    KeyCode::M => b'm',
                    KeyCode::N => b'n',
                    KeyCode::O => b'o',
                    KeyCode::P => b'p',
                    KeyCode::Q => b'q',
                    KeyCode::R => b'r',
                    KeyCode::S => b's',
                    KeyCode::T => b't',
                    KeyCode::U => b'u',
                    KeyCode::V => b'v',
                    KeyCode::W => b'w',
                    KeyCode::X => b'x',
                    KeyCode::Y => b'y',
                    KeyCode::Z => b'z',
                    KeyCode::Key0 | KeyCode::Numpad0 => b'0',
                    KeyCode::Key1 | KeyCode::Numpad1 => b'1',
                    KeyCode::Key2 | KeyCode::Numpad2 => b'2',
                    KeyCode::Key3 | KeyCode::Numpad3 => b'3',
                    KeyCode::Key4 | KeyCode::Numpad4 => b'4',
                    KeyCode::Key5 | KeyCode::Numpad5 => b'5',
                    KeyCode::Key6 | KeyCode::Numpad6 => b'6',
                    KeyCode::Key7 | KeyCode::Numpad7 => b'7',
                    KeyCode::Key8 | KeyCode::Numpad8 => b'8',
                    KeyCode::Key9 | KeyCode::Numpad9 => b'9',
                    KeyCode::Oem2 => b'-',
                    KeyCode::OemComma => b',',
                    KeyCode::OemPeriod => b'.',
                    _ => continue,
                };
                kernel_buffer.push(ch);
            }
        }
    }

    KEY_EVENT_BROADCAST.unsubscribe();

    Ok(kernel_buffer)
}

pub fn sys_open(path_ptr: *const u8, flags: u64) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let path_str = match copy_user_path(&mut buf, path_ptr) {
        Ok(s) => s,
        Err(e) => {
            info.lock().errno = e;
            return -1;
        }
    };

    let path = match resolve_path(path_str, &current_cwd(&info)) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Verify file exists; support create flag.
    // O_APPEND offset is determined per-write by vfs::write, not at open time.
    let append = (flags & 0x400) != 0; // O_APPEND
    let create = (flags & 0x40) != 0; // O_CREAT
    let truncate = (flags & 0x200) != 0; // O_TRUNC
    let offset = 0u64;
    // Parse access mode from the low 2 bits: 0=O_RDONLY, 1=O_WRONLY, 2=O_RDWR.
    let open_mode = match flags & 0x3 {
        0 => OpenMode::ReadOnly,
        1 => OpenMode::WriteOnly,
        2 => OpenMode::ReadWrite,
        _ => OpenMode::ReadWrite, // 3 is not a valid Linux value; treat as ReadWrite
    };
    interrupts::enable();
    match fs_api::file_info(&path) {
        Ok(_) => {
            if truncate {
                if let Err(e) = fs_api::truncate(&path, 0) {
                    info.lock().errno = Errno::from(e);
                    return -1;
                }
            }
        }
        Err(e) => {
            if create {
                if let Err(e) = fs_api::create_file(&path) {
                    info.lock().errno = Errno::from(e);
                    return -1;
                }
            } else {
                info.lock().errno = Errno::from(e);
                return -1;
            }
        }
    }

    // Resolve VFS operation at open time. Cache fs, relative, mount_id, and
    // inode so subsequent read/write syscalls skip the mount-registry scan.
    let (cached_op, inode) = match vfs::resolve(&path) {
        Some(op) => (Some(op.fs_info()), op.inode),
        None => (None, None),
    };

    let desc = FileDescriptor::FsFile(FsFile {
        path,
        offset,
        append,
        mode: open_mode,
        fs: cached_op.as_ref().map(|c| c.fs.clone()),
        relative: cached_op.as_ref().map(|c| c.relative.clone()),
        mount_id: cached_op.as_ref().map(|c| c.mount_id).unwrap_or(0),
        inode,
        ra: crate::fs::readahead::ReadaheadState::default(),
    });
    let fd = info.lock().fd_table.lock().allocate_fd(desc);
    fd as i64
}

pub fn sys_list_dir(path_ptr: *const u8, buffer_ptr: *mut u8, buffer_size: usize) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() || buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    if buffer_size == 0 {
        return 0;
    }

    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let path_str = match copy_user_path(&mut buf, path_ptr) {
        Ok(s) => s,
        Err(e) => {
            info.lock().errno = e;
            return -1;
        }
    };

    let path = match resolve_path(path_str, &current_cwd(&info)) {
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
        Err(e) => {
            info.lock().errno = Errno::from(e);
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
        if !unsafe { try_copy_to_user(user_entry_ptr, entry_bytes.as_ptr(), entry_size) } {
            info.lock().errno = Errno::EFAULT;
            return -1;
        }
        written += entry_size;

        // Copy filename to user buffer
        let user_name_ptr = unsafe { buffer_ptr.add(written) };
        if !unsafe { try_copy_to_user(user_name_ptr, name_bytes.as_ptr(), name_bytes.len()) } {
            info.lock().errno = Errno::EFAULT;
            return -1;
        }
        written += name_bytes.len();
    }

    written as i64
}

pub fn sys_poll(fds_ptr: *mut SelectFd, count: usize, timeout_ms: u64) -> i64 {
    // Cache info before interrupts::enable() to avoid stale per-CPU scheduler
    // reference after thread migration. After enable, use `info` (Arc) directly
    // and call sched() freshly for sleep/park operations.
    let info = current_thread_info();

    if fds_ptr.is_null() && count != 0 {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let timeout = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Duration::from_millis(timeout_ms))
    };

    if count == 0 {
        return 0;
    }

    const MAX_POLL_FDS: usize = 1024;
    if count > MAX_POLL_FDS {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let mut fds = vec![
        SelectFd {
            fd: 0,
            interests: PollState::none(),
            result: PollState::none(),
        };
        count
    ];

    let fds_bytes = count * core::mem::size_of::<SelectFd>();

    if !unsafe { try_copy_from_user(fds.as_mut_ptr() as *mut u8, fds_ptr as *const u8, fds_bytes) }
    {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let copy_back = |entries: &[SelectFd]| unsafe {
        try_copy_to_user(fds_ptr as *mut u8, entries.as_ptr() as *const u8, fds_bytes)
    };

    let descriptors = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        fds.iter()
            .map(|entry| guard.fd_table.lock().get_fd(entry.fd).cloned())
            .collect::<Vec<_>>()
    };

    let thread_weak = match current_thread_weak() {
        Some(w) => w,
        None => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };
    let waiter = Arc::new(PollWaiter::new(thread_weak));

    interrupts::enable();

    let mut contexts: Vec<PollContext> = Vec::with_capacity(count);
    let mut base_ready = 0usize;

    for idx in 0..count {
        let interests = fds[idx].interests;
        fds[idx].result = PollState::none();

        let descriptor = descriptors.get(idx).and_then(|d| d.clone());

        match descriptor {
            None => {
                let entry = &mut fds[idx];
                entry.result.invalid = true;
                entry.result.error = true;
                base_ready += 1;
            }
            Some(FileDescriptor::StandardStream(_)) => {
                let entry = &mut fds[idx];
                entry.result.invalid = true;
                entry.result.error = true;
                base_ready += 1;
            }
            Some(FileDescriptor::PipeRead(pipe) | FileDescriptor::PipeWrite(pipe)) => {
                let pollable: Box<dyn Pollable> = Box::new(PollablePipe::new(pipe.clone()));
                let poll_entry = Arc::new(PollEntry::new(waiter.clone(), interests));
                let registration = pollable.register(poll_entry.clone());
                fds[idx].result = registration.initial;
                contexts.push(PollContext {
                    index: idx,
                    interests,
                    pollable,
                    entry: poll_entry,
                    key: registration.key,
                });
            }
            Some(FileDescriptor::PtyMaster(pty)) => {
                let pollable: Box<dyn Pollable> = Box::new(PollablePtyMaster::new(pty.clone()));
                let poll_entry = Arc::new(PollEntry::new(waiter.clone(), interests));
                let registration = pollable.register(poll_entry.clone());
                fds[idx].result = registration.initial;
                contexts.push(PollContext {
                    index: idx,
                    interests,
                    pollable,
                    entry: poll_entry,
                    key: registration.key,
                });
            }
            Some(FileDescriptor::PtySlave(pty)) => {
                let pollable: Box<dyn Pollable> = Box::new(PollablePtySlave::new(pty.clone()));
                let poll_entry = Arc::new(PollEntry::new(waiter.clone(), interests));
                let registration = pollable.register(poll_entry.clone());
                fds[idx].result = registration.initial;
                contexts.push(PollContext {
                    index: idx,
                    interests,
                    pollable,
                    entry: poll_entry,
                    key: registration.key,
                });
            }
            Some(FileDescriptor::Socket(sock)) => {
                let pollable: Box<dyn Pollable> = Box::new(PollableSocket::new(sock.clone()));
                let poll_entry = Arc::new(PollEntry::new(waiter.clone(), interests));
                let registration = pollable.register(poll_entry.clone());
                fds[idx].result = registration.initial;
                contexts.push(PollContext {
                    index: idx,
                    interests,
                    pollable,
                    entry: poll_entry,
                    key: registration.key,
                });
            }
            Some(FileDescriptor::FsFile(file)) => match fs_api::poll(&file.path) {
                Ok(pollable) => {
                    let poll_entry = Arc::new(PollEntry::new(waiter.clone(), interests));
                    let registration = pollable.register(poll_entry.clone());
                    fds[idx].result = registration.initial;
                    contexts.push(PollContext {
                        index: idx,
                        interests,
                        pollable,
                        entry: poll_entry,
                        key: registration.key,
                    });
                }
                Err(_err) => {
                    let entry = &mut fds[idx];
                    entry.result.error = true;
                    entry.result.invalid = true;
                    base_ready += 1;
                }
            },
        }
    }

    let mut ready = base_ready + refresh_poll_contexts(&mut contexts, &mut fds);
    if ready > 0 {
        cleanup_poll_contexts(&mut contexts);
        if !copy_back(&fds) {
            info.lock().errno = Errno::EFAULT;
            return -1;
        }
        return ready as i64;
    }

    let deadline = timeout.map(|t| Instant::now() + t);
    loop {
        ready = base_ready + refresh_poll_contexts(&mut contexts, &mut fds);
        if ready > 0 {
            break;
        }

        match deadline {
            Some(dl) => {
                let now = Instant::now();
                if now >= dl {
                    break;
                }

                if waiter.arm() {
                    continue;
                }

                // Re-check poll state after arming to close race window.
                // If notification arrived after refresh but before arm,
                // the state was updated before notify() was called.
                ready = base_ready + refresh_poll_contexts(&mut contexts, &mut fds);
                if ready > 0 {
                    break;
                }

                let remaining = dl.duration_since(now);
                let sleep_dur = if remaining.is_zero() {
                    Duration::from_millis(1)
                } else {
                    remaining
                };
                thread_sleep(sleep_dur);
            }
            None => {
                if waiter.arm() {
                    continue;
                }

                thread_park_while(|| {
                    base_ready + refresh_poll_contexts(&mut contexts, &mut fds) == 0
                });
            }
        }
    }

    ready = base_ready + refresh_poll_contexts(&mut contexts, &mut fds);

    cleanup_poll_contexts(&mut contexts);

    if !copy_back(&fds) {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    ready as i64
}

fn refresh_poll_contexts(contexts: &mut [PollContext], fds: &mut [SelectFd]) -> usize {
    let mut ready = 0usize;

    for ctx in contexts.iter() {
        let state = ctx.entry.state();
        fds[ctx.index].result = state;
        if state.matches(ctx.interests) {
            ready += 1;
        }
    }

    ready
}

fn cleanup_poll_contexts(contexts: &mut [PollContext]) {
    for ctx in contexts.iter_mut() {
        if let Some(key) = ctx.key.take() {
            ctx.pollable.unregister(key);
        }
    }
}

pub fn sys_getcwd(buffer_ptr: *mut u8, size: usize) -> i64 {
    let info = current_thread_info();
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
    let cwd_str = current_cwd(&info).to_string();
    let cwd_bytes = cwd_str.as_bytes();

    // Need space for string + null terminator
    if cwd_bytes.len() + 1 > size {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    if !unsafe { try_copy_to_user(buffer_ptr, cwd_bytes.as_ptr(), cwd_bytes.len()) } {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    if !unsafe { try_write_user(buffer_ptr.add(cwd_bytes.len()), 0u8) } {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    (cwd_bytes.len() + 1) as i64
}

pub fn sys_chdir(path_ptr: *const u8) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let path_str = match copy_user_path(&mut buf, path_ptr) {
        Ok(s) => s,
        Err(e) => {
            info.lock().errno = e;
            return -1;
        }
    };

    // Resolve the target path (absolute or relative to current cwd)
    let new_path = match resolve_path(path_str, &current_cwd(&info)) {
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
    set_current_cwd(&info, new_path);
    0
}

const SEEK_SET: u32 = 0;
const SEEK_CUR: u32 = 1;
const SEEK_END: u32 = 2;

/// Read `count` bytes at an explicit `offset`, leaving the fd's own offset alone.
///
/// Threads of one process share a file descriptor table, and with it a single
/// offset per fd, so `lseek` + `read` from two threads races by construction:
/// one thread's seek can land between the other's seek and read. Only regular
/// files have an offset to address; everything else is ESPIPE.
///
/// Readahead state is taken by value and dropped, so a positional read does not
/// steer the sequential-access heuristic of whoever owns the descriptor.
pub fn sys_pread(fd: u64, buffer_ptr: *mut u8, count: usize, offset: u64) -> i64 {
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    // See sys_read: the fd table is a BlockingMutex and several threads share
    // one table, so interrupts have to be back on before it is acquired.
    interrupts::enable();
    let fd_info = fd_table.lock().get_fd(fd).cloned();

    let Some(FileDescriptor::FsFile(file)) = fd_info else {
        info.lock().errno = match fd_info {
            None => Errno::EBADF,
            Some(_) => Errno::ESPIPE,
        };
        return -1;
    };

    let offset = offset as usize;

    if let Some(device) = crate::fs::devfs::try_lookup_from_full_path(&file.path) {
        return match device.read(offset, count) {
            Ok(data) => {
                let bytes_to_copy = data.len().min(count);
                if bytes_to_copy == 0 {
                    return 0;
                }
                if !unsafe { try_copy_to_user(buffer_ptr, data.as_ptr(), bytes_to_copy) } {
                    info.lock().errno = Errno::EFAULT;
                    return -1;
                }
                bytes_to_copy as i64
            }
            Err(_) => {
                info.lock().errno = Errno::EIO;
                -1
            }
        };
    }

    let Some(fs) = file.fs.as_ref() else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };
    let op = vfs::VfsOp {
        fs: fs.clone(),
        // Invariant: relative is Some iff fs is Some (set together at open time).
        relative: file.relative.clone().expect("fs set without relative path"),
        inode: file.inode.clone(),
        mount_id: file.mount_id,
    };

    let mut ra = file.ra;
    match vfs::read_to_user(&op, &mut ra, offset, count, buffer_ptr) {
        Ok(n) => n as i64,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

/// Write `count` bytes at an explicit `offset`, leaving the fd's own offset alone.
///
/// The counterpart to [`sys_pread`], and ESPIPE on anything without an offset.
/// `O_APPEND` is not consulted: POSIX specifies that pwrite writes at the given
/// offset regardless, and a positional write that silently lands somewhere else
/// would defeat the point of the call.
pub fn sys_pwrite(fd: u64, buffer_ptr: *const u8, count: usize, offset: u64) -> i64 {
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    // See sys_read: the fd table is a BlockingMutex and several threads share
    // one table, so interrupts have to be back on before it is acquired.
    interrupts::enable();
    let fd_info = fd_table.lock().get_fd(fd).cloned();

    let Some(FileDescriptor::FsFile(file)) = fd_info else {
        info.lock().errno = match fd_info {
            None => Errno::EBADF,
            Some(_) => Errno::ESPIPE,
        };
        return -1;
    };

    const MAX_WRITE_SIZE: usize = 1024 * 1024; // 1 MiB
    let capped_count = count.min(MAX_WRITE_SIZE);

    let Some(fs) = file.fs.as_ref() else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };
    let op = vfs::VfsOp {
        fs: fs.clone(),
        relative: file.relative.clone().expect("fs set without relative path"),
        inode: file.inode.clone(),
        mount_id: file.mount_id,
    };

    match vfs::write_from_user(&op, offset as usize, buffer_ptr, capped_count, false) {
        Ok(written) => written as i64,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

pub fn sys_lseek(fd: u64, offset: i64, whence: u32) -> i64 {
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    let file = {
        let fd_guard = fd_table.lock();
        match fd_guard.get_fd(fd) {
            Some(FileDescriptor::FsFile(f)) => f.clone(),
            _ => {
                drop(fd_guard);
                info.lock().errno = Errno::EINVAL;
                return -1;
            }
        }
    };

    let new_offset = match whence {
        SEEK_SET => offset,
        SEEK_CUR => file.offset as i64 + offset,
        SEEK_END => {
            let size = match fs_api::file_info(&file.path) {
                Ok(finfo) => finfo.size as i64,
                Err(_) => {
                    info.lock().errno = Errno::EINVAL;
                    return -1;
                }
            };
            size + offset
        }
        _ => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    if new_offset < 0 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let new_fd = FileDescriptor::FsFile(FsFile {
        offset: new_offset as u64,
        ..file
    });
    fd_table.lock().replace_fd(fd, new_fd);

    new_offset
}

/// Returns 1 if the fd refers to a terminal (StandardStream), 0 otherwise.
pub fn sys_isatty(fd: u64) -> u64 {
    let info = current_thread_info();
    let guard = info.lock();
    let fd_table = guard.fd_table.lock();
    match fd_table.get_fd(fd) {
        Some(FileDescriptor::StandardStream(_)) => 1,
        Some(FileDescriptor::PtySlave(_)) => 1,
        _ => 0,
    }
}

pub fn sys_ftruncate(fd: u64, size: u64) -> i32 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let path = {
        let guard = info.lock();
        let fd_table = guard.fd_table.lock();
        match fd_table.get_fd(fd) {
            Some(FileDescriptor::FsFile(f)) => f.path.clone(),
            _ => {
                drop(fd_table);
                drop(guard);
                info.lock().errno = Errno::EINVAL;
                return -1;
            }
        }
    };

    interrupts::enable();
    match fs_api::truncate(&path, size) {
        Ok(()) => 0,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

pub fn sys_fsync(fd: u64) -> i32 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let (path, inode) = {
        let guard = info.lock();
        let fd_table = guard.fd_table.lock();
        match fd_table.get_fd(fd) {
            Some(FileDescriptor::FsFile(f)) => (f.path.clone(), f.inode.clone()),
            _ => {
                drop(fd_table);
                drop(guard);
                info.lock().errno = Errno::EINVAL;
                return -1;
            }
        }
    };

    interrupts::enable();
    if let Err(e) = fs_api::flush_file(&path, inode) {
        info.lock().errno = Errno::from(e);
        return -1;
    }

    // Commit any pending journal transactions so data is durable.
    for journal in BlockPageCache::global().all_journals() {
        if let Err(e) = journal.force_commit_and_wait() {
            log!("sys_fsync: journal commit error: {:?}", e);
            info.lock().errno = Errno::EIO;
            return -1;
        }
    }
    0
}

/// Flush all dirty block cache pages to disk. Always succeeds from the
/// caller's perspective (errors are logged by the writeback thread).
pub fn sys_sync() {
    debug_assert!(
        x86_64::instructions::interrupts::are_enabled(),
        "sys_sync called with interrupts disabled"
    );
    // Commit all pending journal transactions before flushing pages.
    for journal in BlockPageCache::global().all_journals() {
        if let Err(e) = journal.force_commit_and_wait() {
            log!("sys_sync: journal commit error: {:?}", e);
        }
    }
    BlockPageCache::global().sync_all();
}

pub fn sys_rename(old_path_ptr: *const u8, new_path_ptr: *const u8) -> i32 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if old_path_ptr.is_null() || new_path_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let mut buf: PathBuf = [0u8; MAX_PATH_LEN];
    let old_path_str = match copy_user_path(&mut buf, old_path_ptr) {
        Ok(s) => s,
        Err(e) => {
            info.lock().errno = e;
            return -1;
        }
    };

    let old_path = match resolve_path(old_path_str, &current_cwd(&info)) {
        Ok(p) => p,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    let mut buf2: PathBuf = [0u8; MAX_PATH_LEN];
    let new_path_str = match copy_user_path(&mut buf2, new_path_ptr) {
        Ok(s) => s,
        Err(e) => {
            info.lock().errno = e;
            return -1;
        }
    };

    let new_path = match resolve_path(new_path_str, &current_cwd(&info)) {
        Ok(p) => p,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    interrupts::enable();
    match fs_api::rename(&old_path, &new_path) {
        Ok(()) => 0,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

/// Opens a PTY master/slave pair and writes the two fd numbers to user space.
///
/// The user pointer must point to a `[u64; 2]` buffer that receives
/// `[master_fd, slave_fd]`.  Returns 0 on success, `!0u64` on error.
pub fn sys_openpty(pipefd_ptr: *mut [u64; 2]) -> u64 {
    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    if pipefd_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let pty = Arc::new(BlockingMutex::new(Pty::new()));

    let master_fd = info
        .lock()
        .fd_table
        .lock()
        .allocate_fd(FileDescriptor::PtyMaster(pty.clone()));

    let slave_fd = info
        .lock()
        .fd_table
        .lock()
        .allocate_fd(FileDescriptor::PtySlave(pty));

    let fds = [master_fd, slave_fd];
    let fds_bytes = core::mem::size_of_val(&fds);
    if !unsafe { try_copy_to_user(pipefd_ptr as *mut u8, fds.as_ptr() as *const u8, fds_bytes) } {
        info.lock().fd_table.lock().close_fd(master_fd);
        info.lock().fd_table.lock().close_fd(slave_fd);
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    0
}
