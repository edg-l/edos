use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::{string::ToString, vec::Vec};
use core::time::Duration;

use x86_64::instructions::interrupts;

use crate::debug::lock_order::{RANK_PIPE, RANK_PTY};
use crate::fs::block_page_cache::BlockPageCache;
use crate::fs::handle::{PollKey, PollRef, PollSet, Pollable, StaticPoll};
use crate::fs::vfs;
use crate::fs::{Error as FsError, FileKind, PollState, api as fs_api, fifo, path::Path};
use crate::net::socket::{PollableSocket, Socket};
use crate::thread::pipe::{Pipe, PollablePipe};
use crate::thread::poll::PollWaiter;
use crate::thread::preempt::PreemptSpinlock;
use crate::thread::pty::{PollablePtyMaster, PollablePtySlave, PtySlaveRead};
use crate::thread::sched_prof::{self, Stage};
use crate::thread::scheduler::{
    current_thread_info, current_thread_weak, thread_park_while, thread_sleep,
};
use crate::thread::waitqueue::WaitOutcome;
use crate::util::uaccess::{access_ok, try_copy_from_user, try_copy_to_user, try_write_user};
use crate::{
    drivers::{keyboard::KEY_EVENT_BROADCAST, random, tty},
    log, ranked_lock,
    syscalls::{Errno, MAX_PATH_LEN, PathBuf, copy_user_path},
    thread::{
        UserThreadInfo,
        fd::FileDescriptorTable,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        pipe::{FileDescriptor, FsFile, OpenMode, StandardStream},
        pty::Pty,
    },
    timer::Instant,
};

/// Copy `count` bytes out of user space into a kernel buffer, or `None` on fault.
///
/// A user copy can demand fault and park (`handle_demand_fault` runs before the
/// uaccess fixup and may wait on disk I/O). EDOS has no unwinding, so a thread
/// killed while parked there never runs the Drop of any guard its caller holds.
/// Buffering through this helper is what lets a caller do the copy *before*
/// taking a lock rather than under it.
fn copy_in(user_ptr: *const u8, count: usize) -> Option<Vec<u8>> {
    if count == 0 {
        return Some(Vec::new());
    }
    let mut buf = vec![0u8; count];
    if !unsafe { try_copy_from_user(buf.as_mut_ptr(), user_ptr, count) } {
        return None;
    }
    Some(buf)
}

/// Whether `fd` names an open descriptor.
///
/// A transfer of no bytes still has to answer for the descriptor: userspace
/// uses `read(fd, _, 0)` and `write(fd, _, 0)` as a cheap validity probe, so
/// answering 0 for a descriptor that was never open reports a success the call
/// could not have had. Interrupts go back on first for the reason given in
/// [`sys_read`]: the table is a `BlockingMutex` shared between the threads of
/// one process.
fn fd_is_open(fd_table: &Arc<BlockingMutex<FileDescriptorTable>>, fd: u64) -> bool {
    interrupts::enable();
    fd_table.lock().get_fd(fd).is_some()
}

/// How much of a pipe or PTY transfer rides on the kernel stack rather than
/// the heap.
///
/// Both directions need a kernel-side buffer, for the reason [`copy_in`] gives:
/// the user copy cannot happen under the device lock. Below this size that
/// buffer is a stack array, which is what keeps a one-byte round trip between
/// two processes, or a keystroke through a PTY, from allocating at all. Kernel
/// stacks are [`KTHREAD_STACK_SIZE`](crate::memory::KTHREAD_STACK_SIZE), 32 KiB.
///
/// **Sized small on purpose, and the size is the whole point.** Rust zeroes a
/// stack array, so this is a memset on every call however few bytes the call
/// moves, and that memset is what the buffer costs. Measured with
/// `switchbench`'s one-byte pipe echo, a write plus a read:
///
/// | staging buffer | ns |
/// |---|---|
/// | none: a `Vec` per call, as before | 480 |
/// | 2048 B | 480 |
/// | 512 B | 455 |
/// | 128 B | 404 |
///
/// At 2 KiB the memset costs exactly what the allocation it replaced did. Keep
/// it big enough for the transfers that care about latency -- a byte of IPC, a
/// keystroke -- and let anything larger take the heap, where one allocation is
/// amortised over a copy worth making.
const STREAM_STACK_BUF: usize = 128;

/// Most a single pipe or PTY read or write will stage at once. Above it the
/// call is short, which POSIX.1-2024 permits of both `read()` and `write()`,
/// and which is what stops a `write(fd, p, 1 << 30)` from asking the kernel
/// heap for a gigabyte.
const STREAM_MAX_TRANSFER: usize = 1024 * 1024;

/// A kernel staging buffer of `count` bytes, on the stack when it fits.
///
/// `inline` and `heap` are the caller's storage; the returned slice borrows one
/// of them. Only the heap arm allocates, and only above [`STREAM_STACK_BUF`].
fn stage_buffer<'a>(
    inline: &'a mut [u8; STREAM_STACK_BUF],
    heap: &'a mut Vec<u8>,
    count: usize,
) -> &'a mut [u8] {
    let want = count.min(STREAM_MAX_TRANSFER);
    if want <= STREAM_STACK_BUF {
        &mut inline[..want]
    } else {
        *heap = vec![0u8; want];
        heap.as_mut_slice()
    }
}

/// Copy a kernel buffer into user space. Counterpart to [`copy_in`]: the caller
/// releases its guards first, then copies out.
fn copy_out(user_ptr: *mut u8, data: &[u8]) -> bool {
    if data.is_empty() {
        return true;
    }
    unsafe { try_copy_to_user(user_ptr, data.as_ptr(), data.len()) }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct DirEntry {
    pub name_len: u32,     // Length of the filename
    pub file_type: u8,     // 0=File, 1=Directory, 2=Symlink, 3=Special, 4=Fifo
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

impl SelectFd {
    const EMPTY: Self = Self {
        fd: 0,
        interests: PollState::none(),
        result: PollState::none(),
    };
}

/// Descriptor counts at or below this are served from the stack.
const POLL_INLINE_FDS: usize = 8;

/// What a registered descriptor has to be unregistered *through*.
///
/// A registration is only ever undone by the object that took it, and every
/// one of those is already an `Arc` the descriptor table handed us, so naming
/// them costs a refcount rather than the boxed trait object it replaced. Only
/// the filesystem path, which builds its `Pollable` from a path lookup, still
/// has anything to box.
#[derive(Debug)]
enum PollTarget {
    Pipe(Arc<BlockingMutex<Pipe>>),
    PtyMaster(Arc<BlockingMutex<Pty>>),
    PtySlave(Arc<BlockingMutex<Pty>>),
    Socket(Arc<PreemptSpinlock<Socket>>),
    Tty,
    Fs(Box<dyn Pollable>),
}

impl PollTarget {
    fn unregister(&self, key: PollKey) {
        match self {
            Self::Pipe(pipe) => PollablePipe::new(pipe.clone()).unregister(key),
            Self::PtyMaster(pty) => PollablePtyMaster::new(pty.clone()).unregister(key),
            Self::PtySlave(pty) => PollablePtySlave::new(pty.clone()).unregister(key),
            Self::Socket(sock) => PollableSocket::new(sock.clone()).unregister(key),
            Self::Tty => tty::pollable().unregister(key),
            Self::Fs(pollable) => pollable.unregister(key),
        }
    }
}

/// A descriptor whose readiness can still change, so it has to be watched and
/// unregistered.
///
/// Descriptors that register nothing never reach this: their reported state is
/// frozen at registration, so it is written into the caller's array once and
/// counted there.
struct PollContext {
    index: usize,
    slot: usize,
    interests: PollState,
    target: PollTarget,
    key: PollKey,
}

const MAX_RANDOM_LEN: usize = 1 << 20;

fn file_kind_to_u8(kind: FileKind) -> u8 {
    match kind {
        FileKind::File => 0,
        FileKind::Directory => 1,
        FileKind::Symlink => 2,
        FileKind::Special => 3,
        FileKind::Fifo => 4,
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

    cwd.lock().clone()
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
        if !fd_is_open(&fd_table, fd) {
            info.lock().errno = Errno::EBADF;
            return !0u64;
        }
        return 0;
    }
    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    interrupts::enable();

    let (fdinfo, nonblock) = fd_table.lock().get_fd_nonblock(fd);

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
        Some(FileDescriptor::PipeWrite(pipe) | FileDescriptor::PipeReadWrite(pipe)) => {
            // Copy out of user space before taking the pipe lock: a user copy can
            // demand fault and park, and a thread killed while parked never runs
            // the guard's Drop, which would leave the pipe locked for good.
            let probe = sched_prof::now_ns();
            let mut inline = [0u8; STREAM_STACK_BUF];
            let mut heap = Vec::new();
            let data = stage_buffer(&mut inline, &mut heap, count);
            if !unsafe { try_copy_from_user(data.as_mut_ptr(), buffer_ptr, data.len()) } {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            }
            let probe = sched_prof::record(Stage::PipeCopyIn, probe);

            // The pipe is bounded, so a writer with more than fits waits for a
            // reader to take some rather than growing the kernel heap. Loop
            // until it has all gone in, which is what a blocking write means:
            // a short write here would be a partial write userspace never
            // asked for.
            let writer_wq = ranked_lock!(RANK_PIPE, "sys_write::pipe_wq", pipe)
                .writer_wq
                .clone();
            let mut sent = 0usize;
            let written = loop {
                let (written, notif) = {
                    let mut guard = ranked_lock!(RANK_PIPE, "sys_write::pipe", pipe);
                    guard.write(&data[sent..])
                };
                notif.flush();

                let Some(n) = written else {
                    // The last reader went away. Bytes already accepted still
                    // count: POSIX has a write report what it transferred, and
                    // only a write that moved nothing at all is EPIPE.
                    break if sent > 0 { Some(sent) } else { None };
                };
                sent += n;
                if sent == data.len() {
                    break Some(sent);
                }

                // POSIX: a non-blocking write reports what it managed to move,
                // and only one that moved nothing at all is EAGAIN. A write of
                // at most PIPE_BUF is still all or nothing, since `Pipe::write`
                // is what enforces that and it refuses rather than splitting.
                if nonblock {
                    if sent > 0 {
                        break Some(sent);
                    }
                    info.lock().errno = Errno::EAGAIN;
                    return !0u64;
                }

                // Room for what is left, or a reader going away so the write
                // can fail instead. Killable: a full pipe whose reader never
                // reads is a wait only the peer can end, and without this the
                // writer could not be killed while it waited.
                let remaining = data.len() - sent;
                let ready = || {
                    pipe.try_lock()
                        .is_none_or(|guard| guard.write_ready(remaining))
                };
                if writer_wq.wait_until_killable(ready) == WaitOutcome::Killed {
                    info.lock().errno = Errno::EINTR;
                    return !0u64;
                }
            };
            let probe = sched_prof::record(Stage::PipeWrite, probe);
            sched_prof::record(Stage::PipeFlush, probe);
            match written {
                Some(written) => written as u64,
                None => {
                    // POSIX: both, and in this order. The signal is what
                    // terminates a producer that never checks its return
                    // value, and errno is what a producer that does checks.
                    if let Some(tid) = crate::thread::scheduler::current_thread_id() {
                        crate::thread::thread::kill_process_with_signal(
                            tid.0,
                            crate::thread::signal::SIGPIPE,
                        );
                    }
                    info.lock().errno = Errno::EPIPE;
                    !0u64
                }
            }
        }
        Some(FileDescriptor::PipeRead(_)) => {
            info.lock().errno = Errno::EINVAL;
            !0u64
        }
        Some(FileDescriptor::PtyMaster(pty)) => {
            let Some(data) = copy_in(buffer_ptr, count) else {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            };
            let (written, notif) = {
                let mut guard = ranked_lock!(RANK_PTY, "sys_write::pty_master", pty);
                guard.master_write(&data)
            };
            notif.flush();
            written as u64
        }
        Some(FileDescriptor::PtySlave(pty)) => {
            let Some(data) = copy_in(buffer_ptr, count) else {
                info.lock().errno = Errno::EFAULT;
                return !0u64;
            };

            // The output ring is bounded, so a program with more than the
            // terminal can hold waits for the terminal to read rather than
            // growing the kernel heap. Loop until it has all gone in, as the
            // pipe path does: a short write here would be a partial write
            // userspace never asked for.
            let write_wq = ranked_lock!(RANK_PTY, "sys_write::pty_slave_wq", pty).slave_write_wq();
            let mut sent = 0usize;
            let written = loop {
                let (written, notif) = {
                    let mut guard = ranked_lock!(RANK_PTY, "sys_write::pty_slave", pty);
                    guard.slave_write(&data[sent..])
                };
                notif.flush();

                let Some(n) = written else {
                    // The last master went away. Bytes already accepted still
                    // count, and only a write that moved nothing at all is an
                    // error, exactly as for a pipe with no reader.
                    break if sent > 0 { Some(sent) } else { None };
                };
                sent += n;
                if sent == data.len() {
                    break Some(sent);
                }

                // As for a pipe: report what went in, and only a write that
                // moved nothing is EAGAIN.
                if nonblock {
                    if sent > 0 {
                        break Some(sent);
                    }
                    info.lock().errno = Errno::EAGAIN;
                    return !0u64;
                }

                // Killable: a full terminal whose master never reads is a wait
                // only that program can end, and without this the writer could
                // not be killed while it waited.
                let ready = || pty.try_lock().is_none_or(|guard| guard.slave_write_ready());
                if write_wq.wait_until_killable(ready) == WaitOutcome::Killed {
                    info.lock().errno = Errno::EINTR;
                    return !0u64;
                }
            };
            match written {
                Some(written) => written as u64,
                None => {
                    // POSIX: writing to a terminal whose master side is gone is
                    // EIO. No SIGPIPE — that belongs to pipes and sockets, and
                    // a terminal hangup is not a broken pipe.
                    info.lock().errno = Errno::EIO;
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
            let op = vfs::VfsOp::from_open_file(
                fs.clone(),
                // Invariant: relative is Some iff fs is Some (set together at open time).
                file.relative.clone().expect("fs set without relative path"),
                file.inode.clone(),
                file.mount_id,
            );

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
                Err(e) => {
                    // The filesystem's own code, not EINVAL for everything: a
                    // full device and a bad argument are not the same failure,
                    // and a caller that has to tell them apart has only this.
                    info.lock().errno = Errno::from(e);
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

pub fn sys_close(fd: u64) -> i32 {
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
        Some(FileDescriptor::PipeReadWrite(pipe)) => {
            let notif = {
                let mut guard = pipe.lock();
                guard.close_reader_silent();
                guard.close_writer_silent();
                guard.notify_ends()
            };
            notif.flush();
            0
        }
        Some(FileDescriptor::PtyMaster(pty)) => {
            let notif = ranked_lock!(RANK_PTY, "sys_close::pty_master", pty).close_master();
            notif.flush();
            0
        }
        Some(FileDescriptor::PtySlave(pty)) => {
            let notif = ranked_lock!(RANK_PTY, "sys_close::pty_slave", pty).close_slave();
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
            // Read the key under the socket guard and release the entry after
            // it is dropped: the receive path takes the port table before a
            // socket, so the other order is an AB/BA against it.
            let bound = crate::net::socket::port_key(&s);
            let tcp_conn = s.tcp_conn.clone();
            drop(s);
            if let Some(key) = bound {
                crate::net::socket::unbind_port(&sock, key);
            }
            notif.flush();
            // For TCP sockets, send FIN to initiate graceful close. A state
            // with no FIN to send — an outstanding handshake is the reachable
            // one — is aborted instead, so the cleanup sweep collects it rather
            // than the stack retransmitting for a descriptor that is gone.
            if let Some(conn) = tcp_conn {
                let fin = conn.lock().build_fin();
                match fin {
                    Some(fin_seg) => {
                        let remote_ip = conn.lock().remote_ip;
                        if let Some(stack_mutex) = crate::net::stack::NET_STACK.get() {
                            let mut stack = stack_mutex.lock();
                            let _ = stack.send_ip(
                                remote_ip,
                                crate::net::ipv4::IpProtocol::Tcp,
                                &fin_seg,
                            );
                        }
                    }
                    None => conn.lock().abort(),
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
        if !fd_is_open(&fd_table, fd) {
            info.lock().errno = Errno::EBADF;
            return -1;
        }
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
    let (fd_info, nonblock) = fd_table.lock().get_fd_nonblock(fd);

    match fd_info {
        Some(FileDescriptor::StandardStream(stream)) => match stream {
            StandardStream::Stdin => {
                // Stdin reads from keyboard - still needs intermediate buffer
                let kernel_data = match read_from_stdin(count, nonblock) {
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
        Some(FileDescriptor::PipeRead(pipe) | FileDescriptor::PipeReadWrite(pipe)) => {
            let mut inline = [0u8; STREAM_STACK_BUF];
            let mut heap = Vec::new();
            let data = stage_buffer(&mut inline, &mut heap, count);
            // Only fetched if this read actually has to park, which is the slow
            // path by definition.
            let mut reader_wq = None;
            // Block until data is available or all writers are closed (EOF).
            loop {
                // Drain under the guard, copy out after releasing it. Bytes are
                // lost if the copy then faults, which only happens when the
                // caller passed a bad buffer; holding the guard across the copy
                // instead would leak it permanently on a kill.
                let probe = sched_prof::now_ns();
                let (taken, at_eof, notif) = {
                    let mut guard = ranked_lock!(RANK_PIPE, "sys_read::pipe", pipe);
                    let (taken, notif) = guard.read_into(data);
                    (taken, guard.at_eof(), notif)
                };
                let probe = sched_prof::record(Stage::PipeRead, probe);
                notif.flush();
                let probe = sched_prof::record(Stage::PipeFlush, probe);

                if taken > 0 {
                    if !copy_out(buffer_ptr, &data[..taken]) {
                        info.lock().errno = Errno::EFAULT;
                        break -1;
                    }
                    sched_prof::record(Stage::PipeCopyOut, probe);
                    break taken as i64;
                }
                if at_eof {
                    break 0; // EOF: no data and all writers closed
                }
                // An empty pipe with a writer still on it is the wait a
                // non-blocking reader asked not to take.
                if nonblock {
                    info.lock().errno = Errno::EAGAIN;
                    break -1;
                }
                // No data but writer still open: park until woken by write/close.
                // The predicate runs with interrupts off, so it probes the lock
                // rather than taking it and treats a contended pipe as ready --
                // the loop above re-checks under the real lock either way.
                let wq = reader_wq.get_or_insert_with(|| {
                    ranked_lock!(RANK_PIPE, "sys_read::pipe_wq", pipe)
                        .reader_wq
                        .clone()
                });
                if wq.wait_until_killable(|| pipe.try_lock().is_none_or(|guard| guard.readable()))
                    == WaitOutcome::Killed
                {
                    info.lock().errno = Errno::EINTR;
                    break -1;
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
            let mut inline = [0u8; STREAM_STACK_BUF];
            let mut heap = Vec::new();
            let data = stage_buffer(&mut inline, &mut heap, count);
            let (taken, notif) = {
                let mut guard = ranked_lock!(RANK_PTY, "sys_read::pty_master", pty);
                guard.master_read(data)
            };
            notif.flush();

            if taken == 0 {
                // Nothing to read. A descriptor that asked for O_NONBLOCK is
                // told so as EAGAIN; the default answer stays 0, which is what
                // the terminal's poll loop has always read, and which conflates
                // no-data-yet with end of file.
                if nonblock {
                    info.lock().errno = Errno::EAGAIN;
                    return -1;
                }
                return 0;
            }
            if !copy_out(buffer_ptr, &data[..taken]) {
                info.lock().errno = Errno::EFAULT;
                return -1;
            }
            taken as i64
        }
        Some(FileDescriptor::PtySlave(pty)) => {
            let mut inline = [0u8; STREAM_STACK_BUF];
            let mut heap = Vec::new();
            let data = stage_buffer(&mut inline, &mut heap, count);
            // Clone the input_wq Arc before entering the loop (avoids holding lock while blocking).
            let input_wq = ranked_lock!(RANK_PTY, "sys_read::pty_wq", pty).input_wq();
            loop {
                let (result, hangup, notif) = {
                    let mut guard = ranked_lock!(RANK_PTY, "sys_read::pty_slave", pty);
                    let (r, n) = guard.slave_read(data);
                    let hangup = guard.closed_master && guard.input_buf.is_empty();
                    (r, hangup, n)
                };
                notif.flush();

                match result {
                    PtySlaveRead::Data(taken) => {
                        if !copy_out(buffer_ptr, &data[..taken]) {
                            info.lock().errno = Errno::EFAULT;
                            break -1;
                        }
                        break taken as i64;
                    }
                    // Ctrl-D: a zero-length read, which is how POSIX spells EOF.
                    PtySlaveRead::Eof => break 0,
                    PtySlaveRead::WouldBlock => {}
                }
                if hangup {
                    break 0;
                }
                if nonblock {
                    info.lock().errno = Errno::EAGAIN;
                    break -1;
                }
                // Ctrl+C on a terminal read is the ordinary case for this:
                // the thread is killed and nothing else will ever make the
                // predicate true.
                if input_wq.wait_until_killable(|| {
                    pty.try_lock()
                        .is_none_or(|guard| !guard.input_buf.is_empty() || guard.closed_master)
                }) == WaitOutcome::Killed
                {
                    info.lock().errno = Errno::EINTR;
                    break -1;
                }
            }
        }
        Some(FileDescriptor::FsFile(file)) => {
            // Snapshot readahead state before the devfs/vfs branch split.
            let mut ra = file.ra;
            let offset = file.offset as usize;

            // Fast path: devfs devices can be read directly without the FS Mailbox.
            let (bytes_read, ra) =
                if let Some(device) = crate::fs::devfs::try_lookup_from_full_path(&file.path) {
                    // `read_to_user` rather than `read`: a block device's bytes are
                    // already in cached kernel pages, and gathering them into a Vec
                    // for this to copy out of is a second pass over the whole
                    // request. Every other device takes the default, which is the
                    // gather this replaces.
                    match device.read_to_user(offset, count, buffer_ptr) {
                        Ok(n) => {
                            let bytes_to_copy = n.min(count);
                            if bytes_to_copy == 0 {
                                return 0;
                            }
                            (bytes_to_copy, ra) // devfs doesn't mutate ra
                        }
                        Err(e) => {
                            info.lock().errno = Errno::from(crate::fs::Error::from(e));
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
                    let op = vfs::VfsOp::from_open_file(
                        fs.clone(),
                        // Invariant: relative is Some iff fs is Some (set together at open time).
                        file.relative.clone().expect("fs set without relative path"),
                        file.inode.clone(),
                        file.mount_id,
                    );
                    match vfs::read_to_user(&op, &mut ra, offset, count, buffer_ptr) {
                        Ok(n) => (n, ra),
                        Err(e) => {
                            // The filesystem's own error, not a blanket EINVAL:
                            // a caller told "invalid argument" for a failed
                            // fill or a bad address has no way back to what
                            // actually went wrong.
                            info.lock().errno = Errno::from(e);
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
                    // An established connection with an empty receive buffer is
                    // the wait; a closed one falls through to the EOF below.
                    if nonblock {
                        info.lock().errno = Errno::EAGAIN;
                        return -1;
                    }
                    if rx_wq.wait_until_killable(ready) == WaitOutcome::Killed {
                        info.lock().errno = Errno::EINTR;
                        return -1;
                    }
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
                    if nonblock {
                        info.lock().errno = Errno::EAGAIN;
                        return -1;
                    }
                    if rx_wq.wait_until_killable(ready) == WaitOutcome::Killed {
                        info.lock().errno = Errno::EINTR;
                        return -1;
                    }
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

    // Validate before short-circuiting: a zero-length request still carries a
    // flag word, and answering 0 for one this kernel does not implement tells
    // the caller a flag was honoured that never was.
    if count > MAX_RANDOM_LEN || flags != 0 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    if count == 0 {
        return 0;
    }

    let mut kernel_buffer = vec![0u8; count];
    random::fill_bytes(&mut kernel_buffer);

    if !unsafe { try_copy_to_user(buffer_ptr, kernel_buffer.as_ptr(), kernel_buffer.len()) } {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    kernel_buffer.len() as i64
}

fn read_from_stdin(max_count: usize, nonblock: bool) -> Result<alloc::vec::Vec<u8>, i64> {
    use alloc::vec::Vec;
    use pc_keyboard::{KeyCode, KeyState};

    let rx = KEY_EVENT_BROADCAST.subscribe();
    let mut kernel_buffer = Vec::new();

    // Simple keycode→ASCII for raw stdin (no layout, no modifiers).
    // This is a fallback path; the terminal handles real keyboard input.
    while kernel_buffer.len() < max_count {
        // A non-blocking read takes what has been typed and stops there;
        // finding nothing at all is EAGAIN. Note this returns without waiting
        // for the Return the blocking path stops at, so a caller gets a line in
        // pieces.
        let event = if nonblock {
            match rx.try_recv() {
                Some(event) => event,
                None => {
                    if kernel_buffer.is_empty() {
                        current_thread_info().lock().errno = Errno::EAGAIN;
                        return Err(-1);
                    }
                    break;
                }
            }
        } else {
            rx.recv()
        };
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

/// openat(dirfd, path, path_len, flags) -> fd, or -1 on error
///
/// The `*at` form of `open`, taking the path as pointer plus length rather
/// than NUL-terminated. `flags` are `open`'s.
pub fn sys_openat(dirfd: i64, path_ptr: *const u8, path_len: usize, flags: u64) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let path = match super::fs::read_user_path_at(dirfd, path_ptr, path_len) {
        Ok(path) => path,
        Err(err) => {
            info.lock().errno = err;
            return -1;
        }
    };

    open_resolved(&info, path, flags)
}

/// Open without waiting, and read and write without waiting afterwards.
///
/// On a named pipe it also decides the open itself, which is a rendezvous
/// there; see [`crate::fs::fifo`]. Recorded on the descriptor either way, so a
/// later read or write that would block fails with `EAGAIN` instead.
pub const O_NONBLOCK: u64 = 0x800;

/// Writes are placed at the end of the file, resolved per write rather than at
/// open time.
pub const O_APPEND: u64 = 0x400;

/// Every `open` flag this kernel implements: the access mode plus O_CREAT,
/// O_TRUNC, O_APPEND and O_NONBLOCK.
const OPEN_FLAGS_SUPPORTED: u64 = 0x3 | 0x40 | 0x200 | O_APPEND | O_NONBLOCK;

/// The access mode and `O_APPEND` a descriptor was opened with, as `F_GETFL`
/// reports them. Status flags that can be changed afterwards are not here:
/// they live on the descriptor table entry, not on the open file.
pub fn descriptor_open_flags(desc: &FileDescriptor) -> u64 {
    let mode = |m: OpenMode| match m {
        OpenMode::ReadOnly => 0,
        OpenMode::WriteOnly => 1,
        OpenMode::ReadWrite => 2,
    };
    match desc {
        FileDescriptor::StandardStream(StandardStream::Stdin) => 0,
        FileDescriptor::StandardStream(_) => 1,
        FileDescriptor::PipeRead(_) => 0,
        FileDescriptor::PipeWrite(_) => 1,
        // Both ends on one descriptor, a PTY and a socket are all read-write by
        // construction: none of them has an access mode to have been opened
        // with.
        FileDescriptor::PipeReadWrite(_)
        | FileDescriptor::PtyMaster(_)
        | FileDescriptor::PtySlave(_)
        | FileDescriptor::Socket(_) => 2,
        FileDescriptor::FsFile(file) => mode(file.mode) | if file.append { O_APPEND } else { 0 },
    }
}

/// Open an already-resolved absolute path.
fn open_resolved(info: &Arc<IrqSpinlock<UserThreadInfo>>, path: Path, flags: u64) -> i64 {
    // A flag this kernel does not implement is refused rather than ignored:
    // silently dropping O_EXCL or O_DIRECTORY hands back a descriptor whose
    // semantics are not the ones the caller asked for. Access mode 3 has no
    // meaning either.
    if flags & !OPEN_FLAGS_SUPPORTED != 0 || flags & 0x3 == 0x3 {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    // Verify file exists; support create flag.
    // O_APPEND offset is determined per-write by vfs::write, not at open time.
    let append = (flags & O_APPEND) != 0;
    let create = (flags & 0x40) != 0; // O_CREAT
    let truncate = (flags & 0x200) != 0; // O_TRUNC
    let offset = 0u64;
    // Parse access mode from the low 2 bits: 0=O_RDONLY, 1=O_WRONLY, 2=O_RDWR.
    let open_mode = match flags & 0x3 {
        0 => OpenMode::ReadOnly,
        1 => OpenMode::WriteOnly,
        2 => OpenMode::ReadWrite,
        _ => unreachable!(),
    };
    let nonblock = (flags & O_NONBLOCK) != 0;
    interrupts::enable();
    // Everything cached on the descriptor has to name the file the fd refers
    // to, which is not `path` when a symbolic link on it crossed a mount.
    let mut path = path;
    // What was found, which decides whether this open is an ordinary one or a
    // rendezvous on a named pipe. A path that had to be created is a regular
    // file: `O_CREAT` has no way to ask for anything else.
    let mut kind = FileKind::File;
    match fs_api::file_info_resolved(&path) {
        Ok((existing, resolved)) => {
            path = resolved;
            kind = existing.kind;
            // POSIX: O_TRUNC has no effect on anything but a regular file, so
            // `> /dev/klog` must not fail on a filesystem with no truncate.
            if truncate
                && existing.kind == FileKind::File
                && let Err(e) = fs_api::truncate(&path, 0)
            {
                info.lock().errno = Errno::from(e);
                return -1;
            }
        }
        Err(e) => {
            if create {
                // O_CREAT without O_EXCL: another creator winning the race
                // between the lookup above and this call is not an error.
                match fs_api::create_file(&path) {
                    Ok(()) | Err(FsError::AlreadyExists) => {}
                    Err(e) => {
                        info.lock().errno = Errno::from(e);
                        return -1;
                    }
                }
                // `create_file` follows the symbolic links on the path, so the
                // file exists somewhere `path` does not name. Ask again, now
                // that there is something to find.
                if let Ok((_, resolved)) = fs_api::file_info_resolved(&path) {
                    path = resolved;
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

    if kind == FileKind::Fifo {
        // Keyed by inode, so the two ends find each other through the name
        // rather than through the path each of them spelled.
        let Some(ino) = inode.as_ref().map(|i| i.ino) else {
            info.lock().errno = Errno::EIO;
            return -1;
        };
        let mount_id = cached_op.as_ref().map(|c| c.mount_id).unwrap_or(0);
        // This blocks for the peer unless O_NONBLOCK, so it runs with no lock
        // held and with `info` unborrowed.
        return match fifo::open((mount_id, ino), open_mode, nonblock) {
            Ok(desc) => {
                let table = info.lock().fd_table.clone();
                let mut table = table.lock();
                let fd = table.allocate_fd(desc);
                table.set_nonblock(fd, nonblock);
                fd as i64
            }
            Err(errno) => {
                info.lock().errno = errno;
                -1
            }
        };
    }

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
    let table = info.lock().fd_table.clone();
    let mut table = table.lock();
    let fd = table.allocate_fd(desc);
    table.set_nonblock(fd, nonblock);
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

    match write_dir_entries(&files, buffer_ptr, buffer_size, 0) {
        Ok(written) => written as i64,
        Err(e) => {
            info.lock().errno = e;
            -1
        }
    }
}

/// Serialize `files[start..]` into the user buffer as `DirEntry` records, each
/// immediately followed by its `name_len` name bytes. Stops at the first entry
/// that does not fit and returns the number of bytes written.
fn write_dir_entries(
    files: &[crate::fs::File],
    buffer_ptr: *mut u8,
    buffer_size: usize,
    start: usize,
) -> Result<usize, Errno> {
    let mut written = 0usize;
    let entry_size = core::mem::size_of::<DirEntry>();

    for file in files.iter().skip(start) {
        let name_bytes = file.name.as_bytes();

        if written + entry_size + name_bytes.len() > buffer_size {
            break;
        }

        let entry = DirEntry {
            name_len: name_bytes.len() as u32,
            file_type: file_kind_to_u8(file.kind),
            size: file.size,
            attrs: file_attrs_to_u8(file.attrs),
            reserved: [0, 0],
        };

        let entry_bytes = unsafe {
            core::slice::from_raw_parts(&entry as *const DirEntry as *const u8, entry_size)
        };
        let user_entry_ptr = unsafe { buffer_ptr.add(written) };
        if !unsafe { try_copy_to_user(user_entry_ptr, entry_bytes.as_ptr(), entry_size) } {
            return Err(Errno::EFAULT);
        }
        written += entry_size;

        let user_name_ptr = unsafe { buffer_ptr.add(written) };
        if !unsafe { try_copy_to_user(user_name_ptr, name_bytes.as_ptr(), name_bytes.len()) } {
            return Err(Errno::EFAULT);
        }
        written += name_bytes.len();
    }

    Ok(written)
}

/// Read a directory starting at entry index `start`, so a directory larger than
/// the caller's buffer can be enumerated across several calls. Returns the
/// number of bytes written, 0 once `start` is past the last entry, and EINVAL
/// when the entry at `start` alone is too large for the buffer.
pub fn sys_getdents(
    path_ptr: *const u8,
    path_len: usize,
    buffer_ptr: *mut u8,
    buffer_size: usize,
    start: usize,
) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let cwd = current_cwd(&info);
    let path = match super::fs::read_user_path_with_len(path_ptr, path_len, &cwd) {
        Ok(p) => p,
        Err(e) => {
            info.lock().errno = e;
            return -1;
        }
    };

    interrupts::enable();
    let files = match fs_api::list_files(&path) {
        Ok(files) => files,
        Err(e) => {
            info.lock().errno = Errno::from(e);
            return -1;
        }
    };

    if start >= files.len() {
        return 0;
    }

    match write_dir_entries(&files, buffer_ptr, buffer_size, start) {
        // A zero-byte result with entries left means the buffer can never hold
        // the next one; reporting 0 would read as "end of directory" and the
        // caller would silently lose the tail.
        Ok(0) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
        Ok(written) => written as i64,
        Err(e) => {
            info.lock().errno = e;
            -1
        }
    }
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

    // `None` is an unbounded wait; a zero timeout returns without ever waiting.
    let timeout = if timeout_ms == u64::MAX {
        None
    } else {
        Some(Duration::from_millis(timeout_ms))
    };
    let immediate = timeout_ms == 0;

    if count == 0 {
        return 0;
    }

    const MAX_POLL_FDS: usize = 1024;
    if count > MAX_POLL_FDS {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    // Both per-descriptor arrays stay on the stack at the counts poll is
    // almost always called with — a shell watching stdin, a server watching a
    // listener and a few clients — because a heap round trip costs about 41 ns
    // against a whole call of a few hundred.
    let mut inline_fds = [SelectFd::EMPTY; POLL_INLINE_FDS];
    let mut heap_fds;
    let fds: &mut [SelectFd] = if count <= POLL_INLINE_FDS {
        &mut inline_fds[..count]
    } else {
        heap_fds = vec![SelectFd::EMPTY; count];
        &mut heap_fds
    };

    let fds_bytes = count * core::mem::size_of::<SelectFd>();

    if !unsafe { try_copy_from_user(fds.as_mut_ptr() as *mut u8, fds_ptr as *const u8, fds_bytes) }
    {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let copy_back = |entries: &[SelectFd]| unsafe {
        try_copy_to_user(fds_ptr as *mut u8, entries.as_ptr() as *const u8, fds_bytes)
    };

    // The descriptor snapshot stays on the heap. `FileDescriptor` is about a
    // hundred bytes, so an inline array of them costs more to initialise and
    // drop than the single allocation it would save.
    let descriptors = {
        let fd_table = {
            let mut guard = info.lock();
            guard.errno = Errno::Clear;
            guard.fd_table.clone()
        };
        let table = fd_table.lock();
        fds.iter()
            .map(|entry| table.get_fd(entry.fd).cloned())
            .collect::<Vec<_>>()
    };

    let thread_weak = match current_thread_weak() {
        Some(w) => w,
        None => {
            info.lock().errno = Errno::EINVAL;
            return -1;
        }
    };
    // One allocation of slots for the whole call, holding the waiter too.
    let set = Arc::new(PollSet::new(
        PollWaiter::new(thread_weak),
        fds.iter().map(|entry| entry.interests),
    ));

    interrupts::enable();

    let mut contexts: Vec<PollContext> = Vec::new();
    let mut base_ready = 0usize;

    // Records a descriptor whose readiness is settled for the whole call: an
    // invalid one, or one the device registered nothing for.
    let settle = |fds: &mut [SelectFd], idx: usize, state: PollState, ready: &mut usize| {
        fds[idx].result = state;
        if state.matches(fds[idx].interests) {
            *ready += 1;
        }
    };

    for idx in 0..count {
        let interests = fds[idx].interests;
        fds[idx].result = PollState::none();

        let descriptor = descriptors.get(idx).and_then(|d| d.clone());

        // Registers against this descriptor's slot and records the outcome,
        // keeping a context only when the device took a registration that has
        // to be undone. The pollable is built on the stack: only the ones that
        // stay watched are named again, in `PollTarget`.
        macro_rules! register {
            ($pollable:expr, $target:expr) => {{
                let registration = $pollable.register(PollRef::new(&set, idx));
                match registration.key {
                    Some(key) => {
                        fds[idx].result = registration.initial;
                        contexts.push(PollContext {
                            index: idx,
                            slot: idx,
                            interests,
                            target: $target,
                            key,
                        });
                    }
                    None => settle(fds, idx, registration.initial, &mut base_ready),
                }
            }};
        }

        match descriptor {
            None => settle(
                fds,
                idx,
                PollState {
                    invalid: true,
                    error: true,
                    ..PollState::none()
                },
                &mut base_ready,
            ),
            Some(FileDescriptor::StandardStream(stream)) => {
                // These are valid descriptors that read and write, so reporting
                // POLLNVAL turned a select loop over an un-redirected stdin
                // into a spin. Stdin is the console, which can block; the two
                // output streams never do.
                match stream {
                    StandardStream::Stdin => register!(tty::pollable(), PollTarget::Tty),
                    // Never blocks, so it registers nothing and its
                    // readiness is settled here for the whole call.
                    StandardStream::Stdout | StandardStream::Stderr => {
                        let state = PollState {
                            writable: true,
                            ..PollState::none()
                        };
                        StaticPoll::new(state).register(PollRef::new(&set, idx));
                        settle(fds, idx, state, &mut base_ready);
                    }
                }
            }
            Some(
                FileDescriptor::PipeRead(pipe)
                | FileDescriptor::PipeWrite(pipe)
                | FileDescriptor::PipeReadWrite(pipe),
            ) => {
                register!(
                    PollablePipe::new(pipe.clone()),
                    PollTarget::Pipe(pipe.clone())
                )
            }
            Some(FileDescriptor::PtyMaster(pty)) => {
                register!(
                    PollablePtyMaster::new(pty.clone()),
                    PollTarget::PtyMaster(pty.clone())
                )
            }
            Some(FileDescriptor::PtySlave(pty)) => {
                register!(
                    PollablePtySlave::new(pty.clone()),
                    PollTarget::PtySlave(pty.clone())
                )
            }
            Some(FileDescriptor::Socket(sock)) => {
                register!(
                    PollableSocket::new(sock.clone()),
                    PollTarget::Socket(sock.clone())
                )
            }
            Some(FileDescriptor::FsFile(file)) => match fs_api::poll(&file.path) {
                Ok(pollable) => {
                    let registration = pollable.register(PollRef::new(&set, idx));
                    match registration.key {
                        Some(key) => {
                            fds[idx].result = registration.initial;
                            contexts.push(PollContext {
                                index: idx,
                                slot: idx,
                                interests,
                                target: PollTarget::Fs(pollable),
                                key,
                            });
                        }
                        None => settle(fds, idx, registration.initial, &mut base_ready),
                    }
                }
                Err(_err) => settle(
                    fds,
                    idx,
                    PollState {
                        invalid: true,
                        error: true,
                        ..PollState::none()
                    },
                    &mut base_ready,
                ),
            },
        }
    }

    let mut ready = base_ready + refresh_poll_contexts(&set, &contexts, fds);

    // A zero timeout asks what is ready now, and that answer is already
    // computed, so the clock is never consulted: reading it is the most
    // expensive thing in the call.
    if ready == 0 && !immediate {
        // One reading serves both the deadline and the first comparison
        // against it.
        let mut now = Instant::now();
        let deadline = timeout.map(|t| now + t);

        loop {
            ready = base_ready + refresh_poll_contexts(&set, &contexts, fds);
            if ready > 0 {
                break;
            }

            match deadline {
                Some(dl) => {
                    let Some(remaining) = dl.checked_duration_since(now) else {
                        break;
                    };

                    if set.arm() {
                        now = Instant::now();
                        continue;
                    }

                    // Re-check poll state after arming to close race window.
                    // If notification arrived after refresh but before arm,
                    // the state was updated before notify() was called.
                    ready = base_ready + refresh_poll_contexts(&set, &contexts, fds);
                    if ready > 0 {
                        break;
                    }

                    let sleep_dur = if remaining.is_zero() {
                        Duration::from_millis(1)
                    } else {
                        remaining
                    };
                    thread_sleep(sleep_dur);
                    now = Instant::now();
                }
                None => {
                    if set.arm() {
                        continue;
                    }

                    thread_park_while(|| {
                        base_ready + refresh_poll_contexts(&set, &contexts, fds) == 0
                    });
                }
            }
        }

        ready = base_ready + refresh_poll_contexts(&set, &contexts, fds);
    }

    cleanup_poll_contexts(&contexts);

    if !copy_back(fds) {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    ready as i64
}

fn refresh_poll_contexts(set: &PollSet, contexts: &[PollContext], fds: &mut [SelectFd]) -> usize {
    let mut ready = 0usize;

    for ctx in contexts {
        let state = set.state(ctx.slot);
        fds[ctx.index].result = state;
        if state.matches(ctx.interests) {
            ready += 1;
        }
    }

    ready
}

fn cleanup_poll_contexts(contexts: &[PollContext]) {
    for ctx in contexts {
        ctx.target.unregister(ctx.key);
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

    // `size` is the caller's claim about its own buffer. A claim that cannot
    // describe any user buffer is a bad pointer, not a large one, and must be
    // refused before it is compared against the path length below.
    if !access_ok(buffer_ptr as u64, size) {
        info.lock().errno = Errno::EFAULT;
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

    // A transfer of no bytes answers only once the descriptor has been
    // resolved; see `fd_is_open`.
    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    let offset = offset as usize;

    if let Some(device) = crate::fs::devfs::try_lookup_from_full_path(&file.path) {
        return match device.read_to_user(offset, count, buffer_ptr) {
            Ok(n) => n.min(count) as i64,
            Err(e) => {
                info.lock().errno = Errno::from(crate::fs::Error::from(e));
                -1
            }
        };
    }

    let Some(fs) = file.fs.as_ref() else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };
    let op = vfs::VfsOp::from_open_file(
        fs.clone(),
        // Invariant: relative is Some iff fs is Some (set together at open time).
        file.relative.clone().expect("fs set without relative path"),
        file.inode.clone(),
        file.mount_id,
    );

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

    // A transfer of no bytes answers only once the descriptor has been
    // resolved; see `fd_is_open`.
    if count == 0 {
        return 0;
    }

    if buffer_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return -1;
    }

    const MAX_WRITE_SIZE: usize = 1024 * 1024; // 1 MiB
    let capped_count = count.min(MAX_WRITE_SIZE);

    let Some(fs) = file.fs.as_ref() else {
        info.lock().errno = Errno::EINVAL;
        return -1;
    };
    let op = vfs::VfsOp::from_open_file(
        fs.clone(),
        file.relative.clone().expect("fs set without relative path"),
        file.inode.clone(),
        file.mount_id,
    );

    match vfs::write_from_user(&op, offset as usize, buffer_ptr, capped_count, false) {
        Ok(written) => written as i64,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

/// One scatter/gather buffer, laid out as POSIX `struct iovec`.
///
/// The pointer is carried as a `u64` rather than a raw pointer so the array can
/// be copied out of user memory as plain bytes; it is a user address and is
/// only ever handed back to the ordinary read and write paths, which validate
/// it.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IoVec {
    pub base: u64,
    pub len: u64,
}

/// Most buffers one `readv`/`writev` accepts, POSIX `IOV_MAX`.
const IOV_MAX: usize = 1024;

/// Copy a user `iovec` array in, rejecting the shapes POSIX defines as EINVAL:
/// more than `IOV_MAX` entries, or a total length that cannot be returned.
fn copy_in_iovecs(iov_ptr: *const IoVec, iovcnt: usize) -> Result<Vec<IoVec>, Errno> {
    if iovcnt > IOV_MAX {
        return Err(Errno::EINVAL);
    }
    if iov_ptr.is_null() {
        return Err(Errno::EFAULT);
    }
    let mut iovs = vec![IoVec { base: 0, len: 0 }; iovcnt];
    let bytes = iovcnt * core::mem::size_of::<IoVec>();
    if !unsafe { try_copy_from_user(iovs.as_mut_ptr() as *mut u8, iov_ptr as *const u8, bytes) } {
        return Err(Errno::EFAULT);
    }
    let mut total: u64 = 0;
    for iov in &iovs {
        total = match total.checked_add(iov.len) {
            Some(t) if t <= i64::MAX as u64 => t,
            _ => return Err(Errno::EINVAL),
        };
    }
    Ok(iovs)
}

/// Read into a list of buffers, filling each completely before moving to the
/// next. Returns the total transferred, which is short whenever an underlying
/// read is: a short read means the descriptor had nothing more to give, so
/// continuing would either block or skip a gap.
///
/// Each buffer is a separate underlying read, so the buffers are filled in
/// order but the sequence is not atomic against a concurrent reader on the
/// same descriptor.
pub fn sys_readv(fd: u64, iov_ptr: *const IoVec, iovcnt: usize) -> i64 {
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    // An empty vector, or one whose buffers are all empty, never reaches
    // `sys_read`, so the descriptor is validated here instead.
    if !fd_is_open(&fd_table, fd) {
        info.lock().errno = Errno::EBADF;
        return -1;
    }

    if iovcnt == 0 {
        return 0;
    }
    let iovs = match copy_in_iovecs(iov_ptr, iovcnt) {
        Ok(iovs) => iovs,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    let mut total: i64 = 0;
    for iov in &iovs {
        if iov.len == 0 {
            continue;
        }
        let want = iov.len as usize;
        let n = sys_read(fd, iov.base as *mut u8, want);
        if n < 0 {
            // POSIX: an error after a partial transfer is reported as that
            // partial count, and the error is left for the next call to raise.
            if total > 0 {
                info.lock().errno = Errno::Clear;
                return total;
            }
            return n;
        }
        total += n;
        if (n as usize) < want {
            break;
        }
    }
    total
}

/// Write a list of buffers in order, stopping at the first short write. See
/// [`sys_readv`] for the return convention; the same non-atomicity applies, so
/// a `writev` to a pipe shared with another writer can interleave at a buffer
/// boundary.
pub fn sys_writev(fd: u64, iov_ptr: *const IoVec, iovcnt: usize) -> i64 {
    let info = current_thread_info();
    let fd_table = {
        let mut guard = info.lock();
        guard.errno = Errno::Clear;
        guard.fd_table.clone()
    };

    // See `sys_readv`: an all-empty vector never reaches `sys_write`.
    if !fd_is_open(&fd_table, fd) {
        info.lock().errno = Errno::EBADF;
        return -1;
    }

    if iovcnt == 0 {
        return 0;
    }
    let iovs = match copy_in_iovecs(iov_ptr, iovcnt) {
        Ok(iovs) => iovs,
        Err(errno) => {
            info.lock().errno = errno;
            return -1;
        }
    };

    let mut total: i64 = 0;
    for iov in &iovs {
        if iov.len == 0 {
            continue;
        }
        let want = iov.len as usize;
        let n = sys_write(fd, iov.base as *const u8, want);
        if n == !0u64 {
            if total > 0 {
                info.lock().errno = Errno::Clear;
                return total;
            }
            return -1;
        }
        total += n as i64;
        if (n as usize) < want {
            break;
        }
    }
    total
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

/// The PTY behind `fd`, whichever end it names.
fn pty_of_fd(fd: u64) -> Option<alloc::sync::Arc<BlockingMutex<crate::thread::pty::Pty>>> {
    let info = current_thread_info();
    let guard = info.lock();
    let fd_table = guard.fd_table.lock();
    match fd_table.get_fd(fd) {
        Some(FileDescriptor::PtySlave(pty)) | Some(FileDescriptor::PtyMaster(pty)) => {
            Some(pty.clone())
        }
        _ => None,
    }
}

/// Hand the terminal to a process group.
///
/// This is what makes a job "foreground": the line discipline aims Ctrl+C and
/// Ctrl+Z at whichever group holds the terminal, so a shell resuming a job in
/// the foreground gives it the terminal first and takes it back afterwards.
pub fn sys_tcsetpgrp(fd: u64, pgid: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let Some(pty) = pty_of_fd(fd) else {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    };
    ranked_lock!(RANK_PTY, "tcsetpgrp", pty).foreground_pgid = Some(pgid);
    0
}

/// The process group holding the terminal, or 0 if none does.
pub fn sys_tcgetpgrp(fd: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let Some(pty) = pty_of_fd(fd) else {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    };
    ranked_lock!(RANK_PTY, "tcgetpgrp", pty)
        .foreground_pgid
        .unwrap_or(0)
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
    let started = crate::timer::Instant::now();
    if let Err(e) = fs_api::flush_file(&path, inode) {
        // Logged for the same reason as the journal arm below: a failing
        // fsync reaches userspace as a bare errno, and without the kind here
        // there is nothing to attribute it to.
        log!("sys_fsync: flush_file({}) error: {:?}", path, e);
        info.lock().errno = Errno::from(e);
        return -1;
    }
    let flushed = crate::timer::Instant::now();

    // Commit any pending journal transactions so data is durable.
    for journal in BlockPageCache::global().all_journals() {
        if let Err(e) = journal.force_commit_and_wait() {
            log!("sys_fsync: journal commit error: {:?}", e);
            info.lock().errno = Errno::EIO;
            return -1;
        }
    }
    let done = crate::timer::Instant::now();

    // An fsync is expected to cost milliseconds. Anything past a second is a
    // stall worth attributing, and the split says which half owns it.
    let total = done.duration_since(started);
    if total.as_millis() >= FSYNC_SLOW_MS {
        log!(
            "sys_fsync: slow: {} ms total ({} ms flush_file, {} ms journal commit)",
            total.as_millis(),
            flushed.duration_since(started).as_millis(),
            done.duration_since(flushed).as_millis()
        );
    }
    0
}

/// An fsync taking at least this long is logged with its breakdown.
const FSYNC_SLOW_MS: u128 = 1_000;

/// Flush all dirty block cache pages to disk. Always succeeds from the
/// caller's perspective (errors are logged by the writeback thread).
pub fn sys_sync() {
    debug_assert!(
        x86_64::instructions::interrupts::are_enabled(),
        "sys_sync called with interrupts disabled"
    );
    // Commit-then-flush, repeated to a fixed point.
    //
    // A flush pass writes file data out and enrols the metadata that maps it
    // into the journal's active transaction, and writeback refuses to check
    // point a block whose transaction has not committed. So a round always
    // creates work for the next one: stopping at a fixed count leaves that
    // metadata neither committed nor written, `sync` returns with the extents
    // for the data it just wrote still in memory, and the next mount replays a
    // transaction whose blocks never reached their home locations. Bounded so
    // a workload dirtying metadata as fast as we flush cannot spin here
    // forever.
    //
    // The fixed point is `needs_sync_round`, which counts the open transaction
    // as work. The round that flushes the data is the round that fills that
    // transaction, so a test over committed work alone declares victory one
    // round early and leaves every extent the flush allocated in memory.
    const SYNC_MAX_ROUNDS: usize = 8;
    let mut converged = false;
    for _ in 0..SYNC_MAX_ROUNDS {
        for journal in BlockPageCache::global().all_journals() {
            if let Err(e) = journal.force_commit_and_wait() {
                log!("sys_sync: journal commit error: {:?}", e);
            }
        }
        BlockPageCache::global().sync_all();

        // Retire what the flush just checkpointed before asking whether
        // anything is left: `committed_pending` is drained by `advance_tail`
        // and by nothing else, so testing before this can never go false.
        for journal in BlockPageCache::global().all_journals() {
            if let Err(e) = journal.advance_tail() {
                log!("sys_sync: advance_tail error: {:?}", e);
            }
        }

        if !BlockPageCache::global()
            .all_journals()
            .iter()
            .any(|j| j.needs_sync_round())
        {
            converged = true;
            break;
        }
    }
    if !converged {
        // Reaching the cap means `sync` returned with a journal round still
        // owing: a committed transaction the next mount will replay, or an
        // open one holding metadata that has reached no disk at all.
        log!("sys_sync: journal still pending after {SYNC_MAX_ROUNDS} rounds");
    }

    // Publish the tail the flush just earned. Replay starts at the tail
    // recorded in the journal superblock, so a stale tail makes the next mount
    // re-apply transactions whose blocks have since been checkpointed and
    // overwritten -- it reverts good data with older journal copies.
    for journal in BlockPageCache::global().all_journals() {
        if let Err(e) = journal.advance_tail() {
            log!("sys_sync: advance_tail error: {:?}", e);
        }
    }
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

    super::fs::rename_resolved(&old_path, &new_path) as i32
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

    // The fd table is a `BlockingMutex` and its contended path parks, so the
    // `Arc` leaves the thread-info `IrqSpinlock` before it is locked: taken
    // inside that guard, the park would happen with interrupts disabled.
    let fd_table = info.lock().fd_table.clone();

    let (master_fd, slave_fd) = {
        let mut table = fd_table.lock();
        (
            table.allocate_fd(FileDescriptor::PtyMaster(pty.clone())),
            table.allocate_fd(FileDescriptor::PtySlave(pty)),
        )
    };

    let fds = [master_fd, slave_fd];
    let fds_bytes = core::mem::size_of_val(&fds);
    if !unsafe { try_copy_to_user(pipefd_ptr as *mut u8, fds.as_ptr() as *const u8, fds_bytes) } {
        {
            let mut table = fd_table.lock();
            table.close_fd(master_fd);
            table.close_fd(slave_fd);
        }
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    0
}
