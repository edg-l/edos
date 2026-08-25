use crate::thread::preempt::PreemptSpinlock;
use core::{
    arch::naked_asm,
    sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU8, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use alloc::vec;
use alloc::{format, string::ToString, sync::Arc, vec::Vec};
use heapless::Vec as HeaplessVec;
use intrusive_list::Link;
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    registers::{
        control::{Cr3, Cr3Flags, Efer, EferFlags},
        model_specific::{LStar, SFMask, Star},
        rflags::RFlags,
    },
    structures::paging::PageTableFlags,
};

use crate::{
    debug::lock_order::{RANK_MAPPERS, RANK_USER_MM, RANK_VMAS},
    debug::lock_order::{RANK_NET_STACK, RANK_PIPE, RANK_PTY, RANK_SOCKET, RANK_TCP_CONN},
    fs::Error as FsError,
    gdt::selectors,
    loader::TlsTemplate,
    log,
    memory::{
        STACK_ALIGNMENT,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    net::device::NetDevice,
    power, println, ranked_lock,
    syscalls::{
        fs::{
            FstatEntry, UserTimespec, sys_access, sys_faccessat, sys_fstat, sys_fstatat,
            sys_list_mounts, sys_list_partitions, sys_mkdir, sys_mkdirat, sys_mkfifoat, sys_mount,
            sys_readlink, sys_readlinkat, sys_renameat, sys_rmdir, sys_rmdir_all, sys_stat,
            sys_symlink, sys_symlinkat, sys_truncate, sys_unlink, sys_unlinkat, sys_utimensat,
        },
        io::{
            O_NONBLOCK, SelectFd, descriptor_open_flags, sys_chdir, sys_close, sys_getcwd,
            sys_getrandom, sys_list_dir, sys_poll, sys_read, sys_write,
        },
        memory::{sys_mmap, sys_mprotect, sys_msync, sys_munmap},
    },
    thread::{
        UserThreadInfo,
        context::CpuContext,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        pipe::{FileDescriptor, Pipe},
        scheduler::switch_to_kernel_page,
        signal::{self, SignalState},
        thread::{
            State, Thread, ThreadId, allocate_thread_id, deliver_unblocked_signals,
            get_thread_by_id, get_thread_info_by_id, insert_thread, insert_thread_info,
            kill_process_with_signal, process_group_of, set_process_group, signal_process_group,
            take_thread_exit_code,
        },
        util::{kthread_stack_alloc, kthread_stack_free},
    },
    util::uaccess::{
        UAccessError, access_ok, try_copy_string_from_user, try_copy_to_user, try_read_user,
        try_write_user,
    },
};

mod fs;
pub(crate) mod io;
mod ioctl;
pub mod memory;
mod net;
mod profile;
mod shm;
mod sigframe;
mod sync;
mod table;
pub mod trace;
mod window;

use self::ioctl::sys_ioctl;
use self::sync::{sys_futex_wait, sys_futex_wait_pi, sys_futex_wake};
use crate::thread::scheduler::{
    current_thread, current_thread_id, current_thread_info, current_thread_killed,
    current_thread_weak, exit_if_killed, stop_if_signalled, thread_exit, thread_park_while,
    thread_sleep, thread_yield,
};

/// Set the caller's errno and return the `-1` every failing syscall reports.
fn fail_with(errno: Errno) -> u64 {
    current_thread_info().lock().errno = errno;
    !0u64
}

/// Properly decrement refcounts when a FileDescriptor is removed from a table
/// without going through sys_close (e.g. dup2 replacing an existing fd).
fn close_fd_refcount(desc: FileDescriptor) {
    match desc {
        FileDescriptor::PipeRead(pipe) => {
            ranked_lock!(RANK_PIPE, "fd::drop_reader", pipe)
                .close_reader()
                .flush();
        }
        FileDescriptor::PipeWrite(pipe) => {
            ranked_lock!(RANK_PIPE, "fd::drop_writer", pipe)
                .close_writer()
                .flush();
        }
        FileDescriptor::PipeReadWrite(pipe) => {
            let notif = {
                let mut guard = ranked_lock!(RANK_PIPE, "fd::drop_both", pipe);
                guard.close_reader_silent();
                guard.close_writer_silent();
                guard.notify_ends()
            };
            notif.flush();
        }
        FileDescriptor::PtyMaster(pty) => {
            ranked_lock!(RANK_PTY, "fd::drop_master", pty)
                .close_master()
                .flush();
        }
        FileDescriptor::PtySlave(pty) => {
            ranked_lock!(RANK_PTY, "fd::drop_slave", pty)
                .close_slave()
                .flush();
        }
        FileDescriptor::Socket(sock) => {
            let mut s = ranked_lock!(RANK_SOCKET, "fd::drop_socket", sock);
            s.refcount = s.refcount.saturating_sub(1);
            if s.refcount > 0 {
                return; // Other fds still reference this socket
            }
            s.closed = true;
            s.rx_wq.wake_all();
            // The port-table key is read here and released after the socket
            // guard is dropped, never under it. `handle_tcp` holds the port
            // table across a socket lock, so taking them the other way round
            // here is an AB/BA against the receive path: closing a listening
            // socket while a segment arrives for it would wedge both CPUs on
            // preempt spinlocks.
            let bound = crate::net::socket::port_key(&s);
            let tcp_conn = s.tcp_conn.clone();
            drop(s);
            if let Some(key) = bound {
                crate::net::socket::unbind_port(&sock, key);
            }
            // For TCP sockets, send FIN to initiate graceful close
            if let Some(conn) = tcp_conn {
                let fin = ranked_lock!(RANK_TCP_CONN, "fd::drop_fin", conn).build_fin();
                if let Some(fin_seg) = fin {
                    let remote_ip = ranked_lock!(RANK_TCP_CONN, "fd::drop_remote", conn).remote_ip;
                    if let Some(stack_mutex) = crate::net::stack::NET_STACK.get() {
                        let mut stack =
                            ranked_lock!(RANK_NET_STACK, "fd::drop_fin_send", stack_mutex);
                        let _ =
                            stack.send_ip(remote_ip, crate::net::ipv4::IpProtocol::Tcp, &fin_seg);
                    }
                }
            }
        }
        _ => {}
    }
}

/// # Safety
/// Must be called once per core
pub unsafe fn setup_syscall() {
    let s = selectors();

    // STAR register: set kernel/user code segments
    Star::write(
        s.user_code_selector,
        s.user_data_selector,
        s.code_selector,
        s.data_selector,
    )
    .unwrap();

    // LSTAR: syscall entry point
    LStar::write(VirtAddr::new(syscall_entry as *const () as usize as u64));

    // SFMASK: flags to clear on syscall (clear interrupt flag for atomic entry)
    SFMask::write(RFlags::INTERRUPT_FLAG);

    let mut efer = Efer::read();
    efer |= EferFlags::SYSTEM_CALL_EXTENSIONS;
    unsafe { Efer::write(efer) };

    println!("SYSCALL/SYSRET enabled");
}

#[allow(unused)]
#[unsafe(naked)]
unsafe extern "C" fn syscall_entry() {
    /*
        Summary for SYSCALL in x86-64 long mode:

        CPU saves (from user mode):
        RCX ⟵ user RIP (return address)
        R11 ⟵ user RFLAGS

        CPU loads (to kernel mode):

        RIP ⟵ IA32_LSTAR
        CS ⟵ IA32_STAR[47:32]
        SS ⟵ IA32_STAR[47:32] + 8

        RFLAGS ⟵ user_RFLAGS & ~IA32_FMASK (bits set in FMASK get cleared)

        Unchanged by hardware:

        RSP (you must set it)
        RAX, RBX, RDX, RSI, RDI, R8–R10, R12–R15
        FS/GS base registers (but you typically do SWAPGS)
    */
    naked_asm!(
        // Switch to kernel stack
        "swapgs",
        "mov gs:0, rsp",           // Save user RSP
        "mov rsp, gs:8",           // Load kernel stack

        // Build SyscallRegs structure on stack
        "push gs:0", // rsp
        "push r11",                // rflags (RFLAGS saved by syscall)
        "push rcx",                // rip (RIP saved by syscall)

        "push rax",                // syscall number
        "push rdi",                // rdi (arg1)
        "push rsi",                // rsi (arg2)
        "push rdx",                // rdx (arg3)
        "push r8",                 // r8 (arg5)
        "push r9",                 // r9 (arg6)
        "push r10",                // r10 (arg4)
        "push rbx",                // rbx
        "push rbp",                // rbp
        "push r12",                // r12
        "push r13",                // r13
        "push r14",                // r14
        "push r15",                // r15

        // 16 * 8 = 0x80

        // Re-enable interrupts now that we're safely on the kernel stack.
        // SFMASK clears IF on SYSCALL entry for atomic swapgs+stack switch,
        // but the handler itself must run with interrupts enabled so the
        // APIC timer can preempt long syscalls (prevents spinlock deadlocks
        // when a preempted thread holds a lock another syscall needs).
        "sti",

        // Call handler with pointer to SyscallContext
        "mov rdi, rsp",            // Pass pointer to SyscallContext
        "call {handler}",

        // Restore all registers from SyscallContext structure
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rax",

        // Disable interrupts for the swapgs+rsp restore sequence.
        // Must be atomic: an interrupt between swapgs and sysretq would
        // see wrong GS base or user RSP on kernel stack.
        "cli",

        "pop rcx", // RIP
        "pop r11", // RFLAGS
        "pop rsp",

        // Return to user
        "swapgs",
        "sysretq",

        handler = sym syscall_handler,
    );
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SyscallContext {
    // Saved in reverse order (last pushed = first in struct)
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub rbp: u64,
    pub rbx: u64,
    pub r10: u64, // arg4
    pub r9: u64,  // arg6
    pub r8: u64,  // arg5
    pub rdx: u64, // arg3
    pub rsi: u64, // arg2
    pub rdi: u64, // arg1
    pub rax: u64,
    pub rip: u64,    // User RIP
    pub rflags: u64, // User RFLAGS
    pub rsp: u64,
}

const SYS_READ: u64 = 0;
const SYS_WRITE: u64 = 1;
const SYS_CLOSE: u64 = 3;
const SYS_LIST_DIR: u64 = 4;
const SYS_GETDENTS: u64 = 78; // read a directory from an entry index, for continuation
const SYS_GETCWD: u64 = 5;
const SYS_CHDIR: u64 = 6;
const SYS_POLL: u64 = 7;
const SYS_FSTAT: u64 = 8;
const SYS_MMAP: u64 = 9;
const SYS_STAT: u64 = 10;
const SYS_ACCESS: u64 = 21; // check a path against an access mode
const SYS_MUNMAP: u64 = 11;
const SYS_MPROTECT: u64 = 289; // change the protection of an existing mapping
const SYS_LSEEK: u64 = 12;
const SYS_FTRUNCATE: u64 = 13;
const SYS_TRUNCATE: u64 = 76; // resize a file named by path
const SYS_UTIMENSAT: u64 = 280; // stamp a file's access and modification times
const SYS_OPENAT: u64 = 257; // open relative to a directory descriptor
const SYS_MKDIRAT: u64 = 258; // create a directory relative to a directory descriptor
const SYS_MKFIFOAT: u64 = 283; // create a named pipe relative to a directory descriptor
const SYS_FSTATAT: u64 = 262; // stat relative to a directory descriptor
const SYS_UNLINKAT: u64 = 263; // remove a file or directory relative to one
const SYS_RENAMEAT: u64 = 264; // rename between two directory descriptors
const SYS_SYMLINKAT: u64 = 266; // create a symbolic link relative to one
const SYS_READLINKAT: u64 = 267; // read a link's target relative to one
const SYS_FACCESSAT: u64 = 269; // check an access mode relative to one
const SYS_SYMLINK: u64 = 88; // create a symbolic link
const SYS_READLINK: u64 = 89; // read a symbolic link's target
const SYS_FSYNC: u64 = 14;
const SYS_RENAME: u64 = 82;
const SYS_ISATTY: u64 = 15;
const SYS_IOCTL: u64 = 16;
#[allow(unused)]
const SYS_PIPE: u64 = 22;
const SYS_EXIT: u64 = 60;
const SYS_ERRNO: u64 = 0x400;
const SYS_EXECVE: u64 = 59; // replace this process's image
const SYS_FCNTL: u64 = 72; // descriptor flags and duplication
const SYS_PREAD: u64 = 17; // read at an explicit offset
const SYS_PWRITE: u64 = 18; // write at an explicit offset
const SYS_READV: u64 = 19; // read into a list of buffers
const SYS_WRITEV: u64 = 20; // write a list of buffers
const SYS_GETPID: u64 = 39; // get process ID
const SYS_GETUID: u64 = 102; // real user id of the calling process
const SYS_GETGID: u64 = 104; // real group id of the calling process
const SYS_SPAWN: u64 = 57; // spawn process
const SYS_DUP: u64 = 32; // duplicate file descriptor assigning lowest unused fd
const SYS_DUP2: u64 = 33; // duplicate file descriptor to specific target
const SYS_MSYNC: u64 = 34; // flush memory-mapped file pages to storage
const SYS_WAIT_PID: u64 = 40;
const SYS_MOUNT: u64 = 202;
const SYS_LIST_PARTITIONS: u64 = 203;
const SYS_MKDIR: u64 = 204;
const SYS_RMDIR: u64 = 205;
const SYS_RMDIR_ALL: u64 = 206;
const SYS_UNLINK: u64 = 207;
const SYS_LIST_MOUNTS: u64 = 208;
const SYS_SLEEP_MS: u64 = 209;
const SYS_NANOSLEEP: u64 = 35; // sleep with nanosecond-resolution request
const SYS_MONOTONIC_TIME: u64 = 210;
const SYS_CLONE: u64 = 211;
const SYS_FUTEX_WAIT: u64 = 212;
const SYS_FUTEX_WAKE: u64 = 213;
/// `futex_wait`, plus the thread the caller names as the owner of the lock the
/// word stands for, so the wait can lend it a priority.
///
/// A separate number rather than a fourth argument on [`SYS_FUTEX_WAIT`]: a
/// caller built against the three-argument form leaves whatever it likes in
/// `r10`, and reading that as a thread id would boost an unrelated thread
/// chosen by leftover register contents.
const SYS_FUTEX_WAIT_PI: u64 = 317;
/// The calling thread's own id.
///
/// `getpid` answers for the process, so every thread in one shares its value
/// and none of them can name itself. A lock that wants a holder to publish who
/// it is — see [`SYS_FUTEX_WAIT_PI`] — needs the thread, and `sched_setattr`
/// already speaks thread ids it gave userspace no way to obtain.
const SYS_GETTID: u64 = 186;
const SYS_GETRANDOM: u64 = 214;
const SYS_SHM_CREATE: u64 = 215;
const SYS_SHM_MAP: u64 = 216;
const SYS_SHM_UNMAP: u64 = 217;
const SYS_SHM_DESTROY: u64 = 218;
const SYS_WINDOW_CREATE: u64 = 219;
const SYS_WINDOW_DESTROY: u64 = 220;
const SYS_WINDOW_SET: u64 = 221;
const SYS_WINDOW_GET: u64 = 222;
const SYS_WINDOW_POLL: u64 = 223;
const SYS_WINDOW_LIST: u64 = 224;
const SYS_WINDOW_SEND_EVENT: u64 = 225;
const SYS_WINDOW_DAMAGE: u64 = 232;
/// Appoint another process as part of the shell. Init only; see
/// `kernel/src/window/shell.rs`.
const SYS_WINDOW_GRANT_SHELL: u64 = 234;
const SYS_WINDOW_WAIT: u64 = 286;
const SYS_WINDOW_PRESENT: u64 = 287;
const SYS_WINDOW_GRAB_KEY: u64 = 288;
/// Read and write the session clipboard; see `kernel/src/window/clipboard.rs`.
const SYS_CLIPBOARD_GET: u64 = 284;
const SYS_CLIPBOARD_SET: u64 = 285;
/// Return from a signal handler; see `syscalls/sigframe.rs`.
const SYS_SIGRETURN: u64 = 239;
/// Place a process in a process group.
const SYS_SETPGID: u64 = 109;
/// Read a process's process group.
const SYS_GETPGID: u64 = 121;
/// Hand a terminal to a process group, or read which group holds it.
const SYS_TCSETPGRP: u64 = 237;
const SYS_TCGETPGRP: u64 = 238;
/// Claim, release and target the syscall tracer; see `syscalls/trace.rs`.
const SYS_TRACE_CTL: u64 = 235;
/// Drain trace records into the tracer's buffer.
const SYS_TRACE_READ: u64 = 236;
/// Claim, release and interrogate the sampling profiler; see `profile.rs`.
const SYS_PROFILE_CTL: u64 = 318;
/// Drain profile samples into the profiler's buffer.
const SYS_PROFILE_READ: u64 = 319;
const SYS_CLOCK_GETTIME: u64 = 226;
const SYS_OPENPTY: u64 = 227;
const SYS_SPAWN2: u64 = 228;
const SYS_KILL: u64 = 229;
const SYS_SIGACTION: u64 = 230;
const SYS_SIGPROCMASK: u64 = 233;
const SYS_SHM_SIZE: u64 = 231;
const SYS_PING: u64 = 249;
const SYS_NETINFO: u64 = 250;
const SYS_SOCKET: u64 = 240;
const SYS_BIND: u64 = 241;
const SYS_CONNECT: u64 = 242;
const SYS_LISTEN: u64 = 243;
const SYS_ACCEPT: u64 = 244;
const SYS_SENDTO: u64 = 245;
const SYS_RECVFROM: u64 = 246;
const SYS_SHUTDOWN: u64 = 247;
const SYS_SETSOCKOPT: u64 = 248;
const SYS_GETSOCKOPT: u64 = 251;
const SYS_GETPEERNAME: u64 = 252;
const SYS_GETSOCKNAME: u64 = 253;
const SYS_STATFS: u64 = 254;
const SYS_FORK: u64 = 255;
const SYS_GETDNS: u64 = 256;
/// Step the wall clock, for a time client that has just learnt the real time.
const SYS_CLOCK_SETTIME: u64 = 281;
/// Give up the rest of the timeslice, for a caller spinning on state another
/// thread has to produce.
const SYS_SCHED_YIELD: u64 = 282;
/// Set a thread's scheduling attributes: its weight, and the slice it asks for
/// each time it is picked.
const SYS_SCHED_SETATTR: u64 = 314;
/// Read a thread's scheduling attributes back.
const SYS_SCHED_GETATTR: u64 = 315;
/// Point the system resolver at an address, or clear it back to DHCP's.
const SYS_SETDNS: u64 = 316;
const SYS_SYNC: u64 = 162;
const SYS_REBOOT: u64 = 169;

/// `reboot` commands. Flush the filesystems, then: power the machine off,
/// reset it, or stop it where it is.
const REBOOT_POWER_OFF: u64 = 0;
const REBOOT_RESTART: u64 = 1;
const REBOOT_HALT: u64 = 2;

/// Arguments struct for SYS_SPAWN2. Passed as a single pointer from userspace.
#[repr(C)]
#[derive(Clone, Copy)]
struct SpawnArgs {
    path: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
}

// SAFETY: SpawnArgs only holds raw pointers read from userspace; we never
// dereference them outside of the syscall handler on the calling CPU.
unsafe impl Send for SpawnArgs {}
unsafe impl Sync for SpawnArgs {}

extern "C" fn syscall_handler(ctx: *mut SyscallContext) {
    let ctx = unsafe { ctx.as_mut().unwrap() };

    // Beware with some sched() calls, they call hlt which might hang if we don't have interrupts enabled.

    // Note: we may need to call switch_to_kernel_page(); and switch back later.

    // Record the syscall number on the current thread for debug visibility.
    // Single relaxed store on a hot cache line; see Thread::last_syscall.
    //
    // The traced tid is resolved here and carried to the return path in a
    // local: it decides both records, so a thread that marks itself mid-call
    // cannot produce a return with no matching entry, and no reference into
    // per-CPU state outlives this closure.
    let traced_call = crate::util::per_cpu::get_percpu_data()
        .with_current_thread(|t| {
            t.last_syscall.store(ctx.rax as u32, Ordering::Relaxed);
            trace::traced_session(t)
                .map(|generation| trace::TracedCall::new(t.id.0, generation, ctx))
        })
        .flatten();

    if let Some(call) = &traced_call {
        trace::record_enter(call);
    }

    // Each arm overwrites `ctx.rax` with its result, so the number has to be
    // kept if anything after the match wants to name the call.
    let syscall_number = ctx.rax;

    match ctx.rax {
        SYS_WRITE => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *const u8;
            let count = ctx.rdx as usize;
            ctx.rax = sys_write(fd, buffer_ptr, count);
        }
        SYS_READ => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *mut u8;
            let count = ctx.rdx as usize;
            ctx.rax = sys_read(fd, buffer_ptr, count) as u64;
        }
        SYS_IOCTL => {
            let fd = ctx.rdi;
            let request = ctx.rsi;
            let arg = ctx.rdx;
            let arg_len = ctx.r10 as usize;
            let flags = ctx.r8;
            ctx.rax = sys_ioctl(fd, request, arg, arg_len, flags) as u64;
        }
        SYS_CLOSE => {
            let fd = ctx.rdi;
            ctx.rax = sys_close(fd) as u64;
        }
        SYS_ISATTY => {
            let fd = ctx.rdi;
            ctx.rax = io::sys_isatty(fd);
        }
        SYS_PREAD => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *mut u8;
            let count = ctx.rdx as usize;
            let offset = ctx.r10;
            ctx.rax = io::sys_pread(fd, buffer_ptr, count, offset) as u64;
        }
        SYS_PWRITE => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *const u8;
            let count = ctx.rdx as usize;
            let offset = ctx.r10;
            ctx.rax = io::sys_pwrite(fd, buffer_ptr, count, offset) as u64;
        }
        SYS_READV => {
            let fd = ctx.rdi;
            let iov_ptr = ctx.rsi as *const io::IoVec;
            let iovcnt = ctx.rdx as usize;
            ctx.rax = io::sys_readv(fd, iov_ptr, iovcnt) as u64;
        }
        SYS_WRITEV => {
            let fd = ctx.rdi;
            let iov_ptr = ctx.rsi as *const io::IoVec;
            let iovcnt = ctx.rdx as usize;
            ctx.rax = io::sys_writev(fd, iov_ptr, iovcnt) as u64;
        }
        SYS_LSEEK => {
            let fd = ctx.rdi;
            let offset = ctx.rsi as i64;
            let whence = ctx.rdx as u32;
            ctx.rax = io::sys_lseek(fd, offset, whence) as u64;
        }
        SYS_FTRUNCATE => {
            let fd = ctx.rdi;
            let size = ctx.rsi;
            ctx.rax = io::sys_ftruncate(fd, size) as u64;
        }
        SYS_FSYNC => {
            let fd = ctx.rdi;
            ctx.rax = io::sys_fsync(fd) as u64;
        }
        SYS_RENAME => {
            let old_path_ptr = ctx.rdi as *const u8;
            let new_path_ptr = ctx.rsi as *const u8;
            ctx.rax = io::sys_rename(old_path_ptr, new_path_ptr) as u64;
        }
        SYS_LIST_DIR => {
            let path_ptr = ctx.rdi as *const u8;
            let buffer_ptr = ctx.rsi as *mut u8;
            let buffer_size = ctx.rdx as usize;
            ctx.rax = sys_list_dir(path_ptr, buffer_ptr, buffer_size) as u64;
        }
        SYS_GETDENTS => {
            let path_ptr = ctx.rdi as *const u8;
            let path_len = ctx.rsi as usize;
            let buffer_ptr = ctx.rdx as *mut u8;
            let buffer_size = ctx.r10 as usize;
            let start = ctx.r8 as usize;
            ctx.rax = io::sys_getdents(path_ptr, path_len, buffer_ptr, buffer_size, start) as u64;
        }
        SYS_GETCWD => {
            let buffer_ptr = ctx.rdi as *mut u8;
            let size = ctx.rsi as usize;
            ctx.rax = sys_getcwd(buffer_ptr, size) as u64;
        }
        SYS_CHDIR => {
            let path_ptr = ctx.rdi as *const u8;
            ctx.rax = sys_chdir(path_ptr) as u64;
        }
        SYS_POLL => {
            let fds_ptr = ctx.rdi as *mut SelectFd;
            let count = ctx.rsi as usize;
            let timeout = ctx.rdx;
            ctx.rax = sys_poll(fds_ptr, count, timeout) as u64;
        }
        SYS_FSTAT => {
            let fd = ctx.rdi;
            let fstat_buf = ctx.rsi as *mut FstatEntry;
            ctx.rax = sys_fstat(fd, fstat_buf) as u64;
        }
        SYS_STAT => {
            let path_ptr = ctx.rdi as *const u8;
            let path_len = ctx.rsi as usize;
            let fstat_buf = ctx.rdx as *mut FstatEntry;
            ctx.rax = sys_stat(path_ptr, path_len, fstat_buf) as u64;
        }
        SYS_ACCESS => {
            let path_ptr = ctx.rdi as *const u8;
            let path_len = ctx.rsi as usize;
            let mode = ctx.rdx as u32;
            ctx.rax = sys_access(path_ptr, path_len, mode) as u64;
        }
        SYS_TRUNCATE => {
            let path_ptr = ctx.rdi as *const u8;
            let path_len = ctx.rsi as usize;
            let size = ctx.rdx;
            ctx.rax = sys_truncate(path_ptr, path_len, size) as u64;
        }
        SYS_SYMLINK => {
            let target_ptr = ctx.rdi as *const u8;
            let target_len = ctx.rsi as usize;
            let path_ptr = ctx.rdx as *const u8;
            let path_len = ctx.r10 as usize;
            ctx.rax = sys_symlink(target_ptr, target_len, path_ptr, path_len) as u64;
        }
        SYS_READLINK => {
            let path_ptr = ctx.rdi as *const u8;
            let path_len = ctx.rsi as usize;
            let buf = ctx.rdx as *mut u8;
            let buf_len = ctx.r10 as usize;
            ctx.rax = sys_readlink(path_ptr, path_len, buf, buf_len) as u64;
        }
        SYS_OPENAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            let flags = ctx.r10;
            ctx.rax = io::sys_openat(dirfd, path_ptr, path_len, flags) as u64;
        }
        SYS_MKDIRAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            ctx.rax = sys_mkdirat(dirfd, path_ptr, path_len) as u64;
        }
        SYS_MKFIFOAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            ctx.rax = sys_mkfifoat(dirfd, path_ptr, path_len) as u64;
        }
        SYS_UNLINKAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            let flags = ctx.r10;
            ctx.rax = sys_unlinkat(dirfd, path_ptr, path_len, flags) as u64;
        }
        SYS_FSTATAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            let fstat_buf = ctx.r10 as *mut FstatEntry;
            let flags = ctx.r8;
            ctx.rax = sys_fstatat(dirfd, path_ptr, path_len, fstat_buf, flags) as u64;
        }
        SYS_RENAMEAT => {
            let olddirfd = ctx.rdi as i64;
            let old_ptr = ctx.rsi as *const u8;
            let old_len = ctx.rdx as usize;
            let newdirfd = ctx.r10 as i64;
            let new_ptr = ctx.r8 as *const u8;
            let new_len = ctx.r9 as usize;
            ctx.rax = sys_renameat(olddirfd, old_ptr, old_len, newdirfd, new_ptr, new_len) as u64;
        }
        SYS_SYMLINKAT => {
            let target_ptr = ctx.rdi as *const u8;
            let target_len = ctx.rsi as usize;
            let newdirfd = ctx.rdx as i64;
            let path_ptr = ctx.r10 as *const u8;
            let path_len = ctx.r8 as usize;
            ctx.rax = sys_symlinkat(target_ptr, target_len, newdirfd, path_ptr, path_len) as u64;
        }
        SYS_READLINKAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            let buf = ctx.r10 as *mut u8;
            let buf_len = ctx.r8 as usize;
            ctx.rax = sys_readlinkat(dirfd, path_ptr, path_len, buf, buf_len) as u64;
        }
        SYS_FACCESSAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            let mode = ctx.r10 as u32;
            let flags = ctx.r8;
            ctx.rax = sys_faccessat(dirfd, path_ptr, path_len, mode, flags) as u64;
        }
        SYS_UTIMENSAT => {
            let dirfd = ctx.rdi as i64;
            let path_ptr = ctx.rsi as *const u8;
            let path_len = ctx.rdx as usize;
            let times = ctx.r10 as *const UserTimespec;
            let flags = ctx.r8;
            ctx.rax = sys_utimensat(dirfd, path_ptr, path_len, times, flags) as u64;
        }
        SYS_MMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;
            let prot = ctx.rdx as u32;
            let flags = ctx.r10 as u32;
            let r8 = ctx.r8; // phys_addr (MAP_PHYSICAL) or fd (file-backed)
            let r9 = ctx.r9; // file_offset (file-backed only)

            ctx.rax = sys_mmap(addr, length, prot, flags, r8, r9);
        }
        SYS_MUNMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;

            ctx.rax = sys_munmap(addr, length) as u64;
        }
        SYS_MPROTECT => {
            let addr = ctx.rdi;
            let length = ctx.rsi;
            let prot = ctx.rdx as u32;

            ctx.rax = sys_mprotect(addr, length, prot) as u64;
        }
        SYS_MSYNC => {
            let addr = ctx.rdi;
            let len = ctx.rsi;
            let flags = ctx.rdx as u32;
            ctx.rax = sys_msync(addr, len, flags) as u64;
        }
        SYS_EXIT => {
            let code = ctx.rdi as i32;
            if code != 0 {
                log!("exit: process exited with code {}", code);
            }
            thread_exit(code);
        }
        SYS_GETPID => {
            ctx.rax = sys_getpid();
        }
        SYS_GETTID => {
            ctx.rax = current_thread().map_or(0, |t| t.id.0);
        }
        SYS_SCHED_YIELD => {
            thread_yield();
            ctx.rax = 0;
        }
        SYS_SCHED_SETATTR => {
            let tid = ctx.rdi;
            let attr_ptr = ctx.rsi as *const SchedAttr;
            ctx.rax = sys_sched_setattr(tid, attr_ptr);
        }
        SYS_SCHED_GETATTR => {
            let tid = ctx.rdi;
            let attr_ptr = ctx.rsi as *mut SchedAttr;
            ctx.rax = sys_sched_getattr(tid, attr_ptr);
        }
        SYS_GETUID => {
            ctx.rax = current_thread_info().lock().user_id as u64;
        }
        SYS_GETGID => {
            ctx.rax = current_thread_info().lock().group_id as u64;
        }
        SYS_WAIT_PID => {
            let pid = ctx.rdi;
            let flags = ctx.rsi;
            let status_ptr = ctx.rdx as *mut i32;
            ctx.rax = sys_waitpid(pid, flags, status_ptr);
        }
        SYS_ERRNO => {
            ctx.rax = sys_errno();
        }
        SYS_PIPE => {
            let pipefd_ptr = ctx.rdi as *mut [u64; 2];
            ctx.rax = sys_pipe(pipefd_ptr);
        }
        SYS_SPAWN => {
            let path_ptr = ctx.rdi as *const u8;
            let argv_ptr = ctx.rsi as *const *const u8;
            let stdin_fd = ctx.rdx;
            let stdout_fd = ctx.r10;
            let stderr_fd = ctx.r8;
            ctx.rax = sys_spawn(path_ptr, argv_ptr, stdin_fd, stdout_fd, stderr_fd);
        }
        SYS_EXECVE => {
            let path_ptr = ctx.rdi as *const u8;
            let argv = ctx.rsi as *const *const u8;
            let envp = ctx.rdx as *const *const u8;
            ctx.rax = sys_execve(ctx, path_ptr, argv, envp);
        }
        SYS_FCNTL => {
            let fd = ctx.rdi;
            let cmd = ctx.rsi;
            let arg = ctx.rdx;
            ctx.rax = sys_fcntl(fd, cmd, arg) as u64;
        }
        SYS_DUP => {
            let oldfd = ctx.rdi;
            ctx.rax = sys_dup(oldfd);
        }
        SYS_DUP2 => {
            let oldfd = ctx.rdi;
            let newfd = ctx.rsi;
            ctx.rax = sys_dup2(oldfd, newfd);
        }
        SYS_LIST_PARTITIONS => {
            let buffer = ctx.rdi as *mut u8;
            let size = ctx.rsi;
            ctx.rax = sys_list_partitions(buffer, size) as u64;
        }
        SYS_LIST_MOUNTS => {
            let buffer = ctx.rdi as *mut u8;
            let size = ctx.rsi as usize;
            ctx.rax = sys_list_mounts(buffer, size) as u64;
        }
        SYS_STATFS => {
            let path_ptr = ctx.rdi as *const u8;
            let buf = ctx.rsi as *mut u8;
            let buf_len = ctx.rdx as usize;
            ctx.rax = fs::sys_statfs(path_ptr, buf, buf_len) as u64;
        }
        SYS_SLEEP_MS => {
            let milliseconds = ctx.rdi;
            ctx.rax = sys_sleep_ms(milliseconds);
        }
        SYS_NANOSLEEP => {
            let req_ptr = ctx.rdi as *const Timespec;
            let rem_ptr = ctx.rsi as *mut Timespec;
            ctx.rax = sys_nanosleep(req_ptr, rem_ptr);
        }
        SYS_MONOTONIC_TIME => {
            ctx.rax = sys_monotonic_time();
        }
        SYS_MOUNT => {
            let device_id = ctx.rdi;
            let partition_idx = ctx.rsi;
            let path_ptr = ctx.rdx as *const u8;
            let fs_type_ptr = ctx.r10 as *const u8;
            ctx.rax = sys_mount(device_id, partition_idx, path_ptr, fs_type_ptr) as u64;
        }
        SYS_MKDIR => {
            let path_ptr = ctx.rdi as *const u8;
            ctx.rax = sys_mkdir(path_ptr) as u64;
        }
        SYS_RMDIR => {
            let path_ptr = ctx.rdi as *const u8;
            ctx.rax = sys_rmdir(path_ptr) as u64;
        }
        SYS_RMDIR_ALL => {
            let path_ptr = ctx.rdi as *const u8;
            ctx.rax = sys_rmdir_all(path_ptr) as u64;
        }
        SYS_UNLINK => {
            let path_ptr = ctx.rdi as *const u8;
            ctx.rax = sys_unlink(path_ptr) as u64;
        }
        SYS_CLONE => {
            let func_ptr = ctx.rdi;
            let arg = ctx.rsi;
            let flags = ctx.rdx;
            let child_stack = ctx.r10;
            ctx.rax = sys_clone(ctx, func_ptr, arg, flags, child_stack);
        }
        SYS_FUTEX_WAIT => {
            let addr = ctx.rdi as *const u32;
            let expected = ctx.rsi as u32;
            let timeout_ns = ctx.rdx;
            ctx.rax = sys_futex_wait(addr, expected, timeout_ns);
        }
        SYS_FUTEX_WAIT_PI => {
            let addr = ctx.rdi as *const u32;
            let expected = ctx.rsi as u32;
            let timeout_ns = ctx.rdx;
            let owner_tid = ctx.r10;
            ctx.rax = sys_futex_wait_pi(addr, expected, timeout_ns, owner_tid);
        }
        SYS_FUTEX_WAKE => {
            let addr = ctx.rdi as *const u32;
            let count = ctx.rsi as u32;
            ctx.rax = sys_futex_wake(addr, count);
        }
        SYS_GETRANDOM => {
            let buffer_ptr = ctx.rdi as *mut u8;
            let length = ctx.rsi as usize;
            let flags = ctx.rdx;
            ctx.rax = sys_getrandom(buffer_ptr, length, flags) as u64;
        }
        SYS_SHM_CREATE => {
            let size = ctx.rdi;
            ctx.rax = shm::sys_shm_create(size) as u64;
        }
        SYS_SHM_MAP => {
            let shm_id = ctx.rdi;
            let addr_hint = ctx.rsi;
            let prot = ctx.rdx as u32;
            ctx.rax = shm::sys_shm_map(shm_id, addr_hint, prot);
        }
        SYS_SHM_UNMAP => {
            let addr = ctx.rdi;
            ctx.rax = shm::sys_shm_unmap(addr) as u64;
        }
        SYS_SHM_DESTROY => {
            let shm_id = ctx.rdi;
            ctx.rax = shm::sys_shm_destroy(shm_id) as u64;
        }
        SYS_WINDOW_CREATE => {
            let x = ctx.rdi as i64;
            let y = ctx.rsi as i64;
            let width = ctx.rdx;
            let height = ctx.r10;
            ctx.rax = window::sys_window_create(x, y, width, height);
        }
        SYS_WINDOW_DESTROY => {
            let window_id = ctx.rdi;
            ctx.rax = window::sys_window_destroy(window_id);
        }
        SYS_WINDOW_SET => {
            let window_id = ctx.rdi;
            let prop = ctx.rsi;
            let value = ctx.rdx;
            ctx.rax = window::sys_window_set(window_id, prop, value);
        }
        SYS_WINDOW_GET => {
            let window_id = ctx.rdi;
            let prop = ctx.rsi;
            ctx.rax = window::sys_window_get(window_id, prop);
        }
        SYS_WINDOW_POLL => {
            let window_id = ctx.rdi;
            let events_ptr = ctx.rsi as *mut crate::window::WindowEvent;
            let max = ctx.rdx;
            ctx.rax = window::sys_window_poll(window_id, events_ptr, max);
        }
        SYS_WINDOW_LIST => {
            let buffer_ptr = ctx.rdi as *mut u8;
            let max = ctx.rsi;
            let list_flags = ctx.rdx;
            ctx.rax = window::sys_window_list(buffer_ptr, max, list_flags);
        }
        SYS_WINDOW_SEND_EVENT => {
            let window_id = ctx.rdi;
            let event_ptr = ctx.rsi as *const crate::window::WindowEvent;
            ctx.rax = window::sys_window_send_event(window_id, event_ptr);
        }
        SYS_WINDOW_PRESENT => {
            ctx.rax = window::sys_window_present();
        }
        SYS_WINDOW_WAIT => {
            ctx.rax = window::sys_window_wait(ctx.rdi, ctx.rsi, ctx.rdx);
        }
        SYS_WINDOW_DAMAGE => {
            let window_id = ctx.rdi;
            ctx.rax = window::sys_window_damage(
                window_id,
                ctx.rsi as u32,
                ctx.rdx as u32,
                ctx.r10 as u32,
                ctx.r8 as u32,
            );
        }
        SYS_WINDOW_GRAB_KEY => {
            ctx.rax = window::sys_window_grab_key(ctx.rdi, ctx.rsi, ctx.rdx);
        }
        SYS_WINDOW_GRANT_SHELL => {
            ctx.rax = window::sys_window_grant_shell(ctx.rdi);
        }
        SYS_CLIPBOARD_GET => {
            let buffer_ptr = ctx.rsi as *mut u8;
            ctx.rax = window::sys_clipboard_get(ctx.rdi, buffer_ptr, ctx.rdx as usize);
        }
        SYS_CLIPBOARD_SET => {
            let buffer_ptr = ctx.rsi as *const u8;
            ctx.rax = window::sys_clipboard_set(ctx.rdi, buffer_ptr, ctx.rdx as usize);
        }
        SYS_CLOCK_GETTIME => {
            let buf_ptr = ctx.rdi as *mut u8;
            ctx.rax = sys_clock_gettime(buf_ptr);
        }
        SYS_CLOCK_SETTIME => {
            let buf_ptr = ctx.rdi as *const u8;
            ctx.rax = sys_clock_settime(buf_ptr);
        }
        SYS_OPENPTY => {
            let pipefd_ptr = ctx.rdi as *mut [u64; 2];
            ctx.rax = io::sys_openpty(pipefd_ptr);
        }
        SYS_SPAWN2 => {
            let args_ptr = ctx.rdi as *const SpawnArgs;
            ctx.rax = sys_spawn2(args_ptr);
        }
        SYS_KILL => {
            // POSIX addressing: a positive pid is one process, 0 is the
            // caller's group, and a negative pid is the group named by its
            // magnitude. The group forms are how a shell stops a whole job.
            let pid = ctx.rdi as i64;
            let signum = ctx.rsi as u32;
            let info = current_thread_info();
            let delivered = if signum == 0 || signum >= 32 {
                false
            } else if pid > 0 {
                kill_process_with_signal(pid as u64, signum)
            } else {
                let pgid = match pid {
                    0 => current_thread().map(|t| t.pgid()),
                    _ => Some(pid.unsigned_abs()),
                };
                pgid.is_some_and(|pgid| signal_process_group(pgid, signum))
            };
            if delivered {
                ctx.rax = 0;
            } else {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
            }
        }
        SYS_SIGACTION => {
            // `handler` is SIG_DFL, SIG_IGN, or a user function address.
            // `restorer` is the address a handler returns through and is
            // required whenever a real handler is installed: the kernel cannot
            // supply those instructions itself without making a stack
            // executable.
            let signum = ctx.rdi as u32;
            let handler = ctx.rsi;
            let restorer = ctx.rdx;
            let info = current_thread_info();
            if signum == 0
                || signum >= 32
                || signal::is_uncatchable(signum)
                || (handler > signal::SIG_IGN && restorer == 0)
            {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
            } else if let Some(cur_thread) = current_thread() {
                if restorer != 0 {
                    cur_thread
                        .signal
                        .restorer
                        .store(restorer, Ordering::Release);
                }
                let prev = cur_thread.signal.set_handler(signum, handler);
                ctx.rax = prev;
            } else {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
            }
        }
        SYS_SIGRETURN => {
            sigframe::sys_sigreturn(ctx);
        }
        SYS_SIGPROCMASK => {
            // Signal sets are 32 bits wide here, so the mask is passed and the
            // previous one returned by value rather than through pointers.
            let how = ctx.rdi as u32;
            let mask = ctx.rsi as u32;
            let old = current_thread().map(|t| (t.signal.blocked(), t));
            let new = old.as_ref().and_then(|(old, _)| match how {
                signal::SIG_BLOCK => Some(old | mask),
                signal::SIG_UNBLOCK => Some(old & !mask),
                signal::SIG_SETMASK => Some(mask),
                _ => None,
            });
            match (old, new) {
                (Some((old, thread)), Some(new)) => {
                    thread.signal.set_blocked(new);
                    // Whatever the new mask stopped blocking is delivered now.
                    deliver_unblocked_signals(&thread);
                    ctx.rax = old as u64;
                }
                _ => {
                    current_thread_info().lock().errno = Errno::EINVAL;
                    ctx.rax = !0u64;
                }
            }
        }
        SYS_SHM_SIZE => {
            let shm_id = ctx.rdi;
            ctx.rax = shm::sys_shm_size(shm_id) as u64;
        }
        SYS_PING => {
            let dst_ip_ptr = ctx.rdi as *const [u8; 4];
            let id = ctx.rsi as u16;
            let seq = ctx.rdx as u16;
            let timeout_ms = ctx.r10;
            ctx.rax = sys_ping(dst_ip_ptr, id, seq, timeout_ms);
        }
        SYS_SOCKET => {
            let domain = ctx.rdi;
            let sock_type = ctx.rsi;
            let protocol = ctx.rdx;
            ctx.rax = net::sys_socket(domain, sock_type, protocol);
        }
        SYS_BIND => {
            let fd = ctx.rdi;
            let addr_ptr = ctx.rsi as *const net::SockAddrIn;
            let addr_len = ctx.rdx;
            ctx.rax = net::sys_bind(fd, addr_ptr, addr_len);
        }
        SYS_CONNECT => {
            let fd = ctx.rdi;
            let addr_ptr = ctx.rsi as *const net::SockAddrIn;
            let addr_len = ctx.rdx;
            ctx.rax = net::sys_connect(fd, addr_ptr, addr_len);
        }
        SYS_LISTEN => {
            let fd = ctx.rdi;
            let backlog = ctx.rsi as u32;
            ctx.rax = net::sys_listen(fd, backlog);
        }
        SYS_ACCEPT => {
            let fd = ctx.rdi;
            let addr_ptr = ctx.rsi as *mut net::SockAddrIn;
            let addr_len_ptr = ctx.rdx as *mut u32;
            ctx.rax = net::sys_accept(fd, addr_ptr, addr_len_ptr);
        }
        SYS_SENDTO => {
            let fd = ctx.rdi;
            let buf_ptr = ctx.rsi as *const u8;
            let len = ctx.rdx;
            let flags = ctx.r10;
            let addr_ptr = ctx.r8 as *const net::SockAddrIn;
            let addr_len = ctx.r9;
            ctx.rax = net::sys_sendto(fd, buf_ptr, len, flags, addr_ptr, addr_len);
        }
        SYS_RECVFROM => {
            let fd = ctx.rdi;
            let buf_ptr = ctx.rsi as *mut u8;
            let len = ctx.rdx;
            let flags = ctx.r10;
            let addr_ptr = ctx.r8 as *mut net::SockAddrIn;
            let addr_len_ptr = ctx.r9 as *mut u32;
            ctx.rax = net::sys_recvfrom(fd, buf_ptr, len, flags, addr_ptr, addr_len_ptr);
        }
        SYS_NETINFO => {
            let buf_ptr = ctx.rdi as *mut u8;
            let buf_len = ctx.rsi as usize;
            ctx.rax = sys_netinfo(buf_ptr, buf_len);
        }
        SYS_SHUTDOWN => {
            ctx.rax = net::sys_shutdown(ctx.rdi, ctx.rsi);
        }
        SYS_SETSOCKOPT => {
            ctx.rax = net::sys_setsockopt(
                ctx.rdi,
                ctx.rsi as i32,
                ctx.rdx as i32,
                ctx.r10 as *const u8,
                ctx.r8 as u32,
            );
        }
        SYS_GETSOCKOPT => {
            ctx.rax = net::sys_getsockopt(
                ctx.rdi,
                ctx.rsi as i32,
                ctx.rdx as i32,
                ctx.r10 as *mut u8,
                ctx.r8 as *mut u32,
            );
        }
        SYS_GETPEERNAME => {
            ctx.rax = net::sys_getpeername(
                ctx.rdi,
                ctx.rsi as *mut net::SockAddrIn,
                ctx.rdx as *mut u32,
            );
        }
        SYS_GETSOCKNAME => {
            ctx.rax = net::sys_getsockname(
                ctx.rdi,
                ctx.rsi as *mut net::SockAddrIn,
                ctx.rdx as *mut u32,
            );
        }
        SYS_FORK => {
            ctx.rax = sys_fork(ctx) as u64;
        }
        SYS_GETDNS => {
            ctx.rax = net::sys_getdns(ctx.rdi as *mut [u8; 4]);
        }
        SYS_SETDNS => {
            ctx.rax = net::sys_setdns(ctx.rdi as *const [u8; 4]);
        }
        SYS_SYNC => {
            io::sys_sync();
            ctx.rax = 0;
        }
        SYS_REBOOT => {
            ctx.rax = sys_reboot(ctx.rdi);
        }
        SYS_SETPGID => {
            let pid = ctx.rdi;
            let pgid = ctx.rsi;
            ctx.rax = match current_thread_id() {
                Some(caller) => match set_process_group(pid, pgid, caller.0) {
                    Ok(()) => 0,
                    Err(errno) => fail_with(errno),
                },
                None => fail_with(Errno::EINVAL),
            };
        }
        SYS_GETPGID => {
            let pid = ctx.rdi;
            ctx.rax = match current_thread_id() {
                Some(caller) => match process_group_of(pid, caller.0) {
                    Ok(pgid) => pgid,
                    Err(errno) => fail_with(errno),
                },
                None => fail_with(Errno::EINVAL),
            };
        }
        SYS_TCSETPGRP => {
            let fd = ctx.rdi;
            let pgid = ctx.rsi;
            ctx.rax = io::sys_tcsetpgrp(fd, pgid);
        }
        SYS_TCGETPGRP => {
            let fd = ctx.rdi;
            ctx.rax = io::sys_tcgetpgrp(fd);
        }
        SYS_TRACE_CTL => {
            let op = ctx.rdi;
            let arg = ctx.rsi;
            ctx.rax = trace::sys_trace_ctl(op, arg);
        }
        SYS_TRACE_READ => {
            let buf = ctx.rdi as *mut edos_trace_abi::TraceRecord;
            let max = ctx.rsi;
            let timeout_ms = ctx.rdx;
            ctx.rax = trace::sys_trace_read(buf, max, timeout_ms);
        }
        SYS_PROFILE_CTL => {
            let op = ctx.rdi;
            let arg = ctx.rsi;
            ctx.rax = profile::sys_profile_ctl(op, arg);
        }
        SYS_PROFILE_READ => {
            let buf = ctx.rdi as *mut edos_profile_abi::Sample;
            let max = ctx.rsi;
            let timeout_ms = ctx.rdx;
            ctx.rax = profile::sys_profile_read(buf, max, timeout_ms);
        }
        _ => {
            current_thread_info().lock().errno = Errno::ENOSYS;
            ctx.rax = !0u64;
        }
    }

    // A failure leaves the entry as a negated errno, which is what every C
    // library expects: a return in `[-4095, -1]` is an error code and anything
    // else is a result. Bounding the window is what lets a call return a
    // pointer or a count with its top bit set without being read as a failure.
    //
    // The substitution happens here rather than in each implementation because
    // all of them already report failure the same way — a bare `-1` with the
    // code left on the thread — so there is one place to change instead of 117.
    // The thread's errno stays set as well: `SYS_ERRNO` still answers from it,
    // and a runtime that has not moved off that call keeps working.
    if ctx.rax == u64::MAX {
        let mut code = current_thread_info().lock().errno;
        if code == Errno::Clear {
            // A path that failed without saying why. Reporting UNKNOWN is worse
            // than the truth and better than -1, which a libc reads as EPERM.
            log!("syscall {syscall_number} returned -1 with no errno set");
            code = Errno::UNKNOWN;
        }
        ctx.rax = (code as u64).wrapping_neg();
    }

    if let Some(call) = &traced_call {
        trace::record_exit(call, ctx.rax);
    }

    // A thread marked for termination dies here rather than returning to user
    // code, and one carrying a stop signal suspends here. This is one of the
    // two boundaries where either is safe; the other is a timer tick that
    // interrupted ring 3, which covers a thread that spins without ever making
    // a syscall. Stop first, so a process resumed by SIGCONT and killed while
    // suspended still dies on the way out.
    stop_if_signalled();
    exit_if_killed();

    // Last, because it rewrites the very context the stub is about to restore
    // from, and because a thread that is dying or suspended has no business
    // running user code. A handled signal outlives both checks: neither marks
    // the thread killed.
    sigframe::deliver_pending_handler(ctx);
}

pub fn sys_errno() -> u64 {
    current_thread_info().lock().errno as u64
}

/// Writes nanoseconds since the Unix epoch as a little-endian `u64` into the
/// caller's 8-byte buffer.
///
/// Answered from the monotonic counter plus the wall-clock offset sampled at
/// boot, so the call costs a counter read rather than the several port
/// round-trips an RTC read takes, and it carries a date and sub-second
/// resolution that a raw RTC reading does not.
fn sys_clock_gettime(buf_ptr: *mut u8) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if buf_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let Some(nanos) = crate::timer::wall_clock_nanos() else {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    };

    let data = nanos.to_le_bytes();
    if !unsafe { try_copy_to_user(buf_ptr, data.as_ptr(), 8) } {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    0
}

/// Steps the wall clock to the nanoseconds since the Unix epoch held in the
/// caller's 8-byte little-endian buffer.
///
/// The RTC is sampled once at boot and every later answer is that reading plus
/// HPET ticks, so the clock is only ever as good as one one-second-resolution
/// sample; this is how a time client corrects it. Only the wall clock moves —
/// the monotonic counter durations are measured against is untouched.
fn sys_clock_settime(buf_ptr: *const u8) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if buf_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let Some(nanos) = (unsafe { try_read_user(buf_ptr as *const u64) }) else {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    };

    if !crate::timer::set_wall_clock_nanos(nanos) {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    log!("clock: wall clock stepped to {} ns since the epoch", nanos);
    0
}

/// Error codes as userspace sees them.
///
/// The discriminants are POSIX's, matching Linux's `asm-generic/errno.h`, so a
/// C library ported to this system reads them straight through instead of
/// carrying a translation table that has to be kept in step as codes are
/// added. Nothing about the numbering is load-bearing for the kernel itself;
/// it is chosen for the port's benefit.
///
/// The value is ABI twice over: `SYS_ERRNO` returns it, and a failing syscall
/// returns its negation. Every code therefore has to fit the `[-4095, -1]`
/// window that separates an error return from a large valid one, which is what
/// `UNKNOWN` sits at the far end of.
///
/// Codes with no producer yet are here so that a port has something to map to.
/// `ENOSYS` in particular is what an unimplemented stub should report rather
/// than inventing a nearby lie, and C99 `math.h` requires `EDOM` and `ERANGE`.
macro_rules! errnos {
    ($($(#[$attr:meta])* $name:ident = $value:expr,)*) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[allow(clippy::upper_case_acronyms, unused)]
        #[repr(u64)]
        pub enum Errno {
            $($(#[$attr])* $name = $value,)*
        }

        /// Every `Errno`, so the values and names reach userspace without a
        /// second copy of the list.
        pub const ALL_ERRNOS: &[Errno] = &[$(Errno::$name,)*];

        impl Errno {
            pub fn name(self) -> &'static str {
                match self {
                    $(Errno::$name => stringify!($name),)*
                }
            }
        }
    };
}

errnos! {
    /// No error; used to clear the errno field.
    Clear = 0,
    /// Operation not permitted for the current caller.
    EPERM = 1,
    /// Requested file or directory does not exist.
    ENOENT = 2,
    /// No such process.
    ESRCH = 3,
    /// System call interrupted (e.g. by a signal or kill).
    EINTR = 4,
    /// Generic I/O failure surfaced from the filesystem or storage layer.
    EIO = 5,
    /// No such device or address: a named pipe opened for writing with
    /// `O_NONBLOCK` and no reader on the other side.
    ENXIO = 6,
    /// Argument list too long.
    E2BIG = 7,
    /// File format is not recognized as an executable.
    ENOEXEC = 8,
    /// Invalid or closed file descriptor.
    EBADF = 9,
    /// No child processes to wait for.
    ECHILD = 10,
    /// Resource temporarily unavailable; operation would block.
    EAGAIN = 11,
    /// Memory allocation failed or memory exhausted.
    ENOMEM = 12,
    /// Operation requires permissions the caller lacks.
    EACCES = 13,
    /// Bad memory address provided by userspace.
    EFAULT = 14,
    /// Device or resource is in use, e.g. a disk that backs a live mount.
    EBUSY = 16,
    /// Attempted to create an entry that already exists.
    EEXIST = 17,
    /// Link or rename across two filesystems.
    EXDEV = 18,
    /// No such device.
    ENODEV = 19,
    /// Expected a directory but encountered a non-directory entry.
    ENOTDIR = 20,
    /// Operation required a regular file but encountered a directory.
    EISDIR = 21,
    /// Invalid argument passed to a syscall.
    EINVAL = 22,
    /// Too many open files in the system.
    ENFILE = 23,
    /// Too many open files in this process.
    EMFILE = 24,
    /// Inappropriate ioctl for the device.
    ENOTTY = 25,
    /// File too large.
    EFBIG = 27,
    /// Device or filesystem has no space left for the operation.
    ENOSPC = 28,
    /// Seek on a descriptor that has no file offset (pipe, socket, tty).
    ESPIPE = 29,
    /// Write attempted on a read-only filesystem or device.
    EROFS = 30,
    /// Too many links.
    EMLINK = 31,
    /// Broken pipe: write to a closed connection.
    EPIPE = 32,
    /// Argument outside a mathematical function's domain. Required by C99
    /// `math.h`; no kernel path produces it.
    EDOM = 33,
    /// Result outside the representable range. Required by C99 `math.h`.
    ERANGE = 34,
    /// Path or component too long.
    ENAMETOOLONG = 36,
    /// Function not implemented. What a stub should report rather than
    /// choosing a nearby code that means something else.
    ENOSYS = 38,
    /// Directory not empty.
    ENOTEMPTY = 39,
    /// Too many symbolic links were traversed resolving one path.
    ELOOP = 40,
    /// Value too large for its type.
    EOVERFLOW = 75,
    /// Socket operation on something that is not a socket.
    ENOTSOCK = 88,
    /// Message too long for the transport.
    EMSGSIZE = 90,
    /// Operation not supported on this object.
    EOPNOTSUPP = 95,
    /// Address family not supported (e.g. IPv6 on IPv4-only system).
    EAFNOSUPPORT = 97,
    /// Address already in use (bind).
    EADDRINUSE = 98,
    /// Address not available on any local interface.
    EADDRNOTAVAIL = 99,
    /// No route to the network.
    ENETUNREACH = 101,
    /// Connection aborted before it was established.
    ECONNABORTED = 103,
    /// Connection reset by the peer.
    ECONNRESET = 104,
    /// No buffer space available.
    ENOBUFS = 105,
    /// The socket is already connected.
    EISCONN = 106,
    /// Socket is not connected.
    ENOTCONN = 107,
    /// Operation timed out.
    ETIMEDOUT = 110,
    /// Connection was refused by the remote host.
    ECONNREFUSED = 111,
    /// No route to the host.
    EHOSTUNREACH = 113,
    /// A `connect` is already under way on this socket.
    EALREADY = 114,
    /// A non-blocking `connect` has started its handshake and has not finished.
    EINPROGRESS = 115,
    /// A kernel error with no code of its own. Sits at the top of the error
    /// window so it can never collide with a code POSIX may add.
    UNKNOWN = 4095,
}

impl From<FsError> for Errno {
    fn from(err: FsError) -> Self {
        match err {
            FsError::FileNotFound => Errno::ENOENT,
            FsError::NotAFile => Errno::EISDIR,
            FsError::NotADir => Errno::ENOTDIR,
            FsError::IoError => Errno::EIO,
            FsError::MissingCriticalSectors => Errno::EIO,
            FsError::AhciError(_) => Errno::EIO,
            FsError::InvalidFs => Errno::EINVAL,
            FsError::Corrupted => Errno::EIO,
            FsError::Unsupported => Errno::EIO,
            FsError::Busy => Errno::EBUSY,
            FsError::InvalidArgument => Errno::EINVAL,
            FsError::TooManyLinks | FsError::LinkEscape => Errno::ELOOP,
            FsError::AlreadyExists => Errno::EEXIST,
            FsError::NoSpace => Errno::ENOSPC,
            FsError::ProtocolMismatch => Errno::EIO,
        }
    }
}

fn sys_getpid() -> u64 {
    current_thread_info().lock().pid
}

/// What a thread asks the scheduler for, as userspace states it.
///
/// Two dials and they are not the same dial: `priority` selects the *weight*,
/// which is a share of the CPU, and `slice_ns` is a *request*, which is how
/// long a turn lasts and so how soon the next one comes. Asking for a shorter
/// slice buys latency at the price of switches and takes bandwidth from nobody;
/// asking for a higher priority takes bandwidth from everyone. This is the knob
/// EEVDF exists to provide, and it is `sched_attr::sched_runtime` in the shape
/// this kernel has a use for.
#[repr(C)]
#[derive(Clone, Copy)]
struct SchedAttr {
    /// `0..runqueue::PRIORITY_LEVELS`, saturating at the top.
    priority: u32,
    _pad: u32,
    /// Nanoseconds of service per pick, held to `MIN_SLICE ..= MAX_SLICE`.
    /// `sched_getattr` reports what was actually set, not what was asked for.
    slice_ns: u64,
}

/// Resolve a `tid` argument, where 0 names the calling thread.
fn sched_attr_target(tid: u64) -> Option<Arc<Thread>> {
    if tid == 0 {
        return current_thread();
    }
    get_thread_by_id(ThreadId(tid))
}

/// There is no privilege check here because this system has no user model to
/// check against, and EEVDF is what makes that tolerable: the worst a thread
/// can do to another with the top of the table is take 6x its share, which is a
/// share and not a lockout. The slice is clamped rather than rejected on both
/// sides, so a program written against a different scheduler's range gets the
/// nearest thing this one will serve instead of an error it has no answer for.
fn sys_sched_setattr(tid: u64, attr_ptr: *const SchedAttr) -> u64 {
    current_thread_info().lock().errno = Errno::Clear;

    let Some(attr) = (unsafe { try_read_user(attr_ptr) }) else {
        return fail_with(Errno::EFAULT);
    };
    let Some(thread) = sched_attr_target(tid) else {
        return fail_with(Errno::ESRCH);
    };

    thread.set_slice_ns(attr.slice_ns);
    thread.set_priority(attr.priority.min(u8::MAX as u32) as u8);
    0
}

fn sys_sched_getattr(tid: u64, attr_ptr: *mut SchedAttr) -> u64 {
    current_thread_info().lock().errno = Errno::Clear;

    let Some(thread) = sched_attr_target(tid) else {
        return fail_with(Errno::ESRCH);
    };
    let attr = SchedAttr {
        priority: thread.priority() as u32,
        _pad: 0,
        slice_ns: thread.request_ns(),
    };
    if !unsafe { try_write_user(attr_ptr, attr) } {
        return fail_with(Errno::EFAULT);
    }
    0
}

/// `waitpid` flags. Blocking is the low bit; `UNTRACED` asks to hear about a
/// child that stopped as well as one that exited, which is what lets a shell
/// notice Ctrl+Z without polling procfs.
const WAIT_BLOCK: u64 = 1;
const WAIT_UNTRACED: u64 = 2;

/// Status value reported for a stopped child, distinguishable from any exit
/// code: an exit status is a byte, this is not.
const STATUS_STOPPED: i32 = 0x1_0000;

fn sys_waitpid(pid: u64, flags: u64, status_ptr: *mut i32) -> u64 {
    use crate::thread::thread::EXITED_THREADS;

    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let target = ThreadId(pid);
    let block = flags & WAIT_BLOCK != 0;
    let untraced = flags & WAIT_UNTRACED != 0;
    // Level-triggered on purpose: an untraced wait answers for as long as the
    // child is down, so a caller can use it to ask "is it still stopped?" and
    // get the same answer twice. `programs/sigtest` checks exactly that, and
    // `edos-sh` polls it the same way. POSIX's "status not yet reported" would
    // make the second query block instead, which is not what anything here
    // wants from it.
    let has_stopped =
        || untraced && get_thread_by_id(target).is_some_and(|t| t.stopped.load(Ordering::Acquire));

    // Fast path: already exited
    if let Some(code) = take_thread_exit_code(target) {
        if !status_ptr.is_null() && !unsafe { try_write_user(status_ptr, code) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        return pid;
    }

    // A stopped child is reported before the blocking wait, because it is not
    // going to exit while it is suspended and a caller that blocked here would
    // never come back.
    if has_stopped() {
        if !status_ptr.is_null() && !unsafe { try_write_user(status_ptr, STATUS_STOPPED) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        return pid;
    }

    if !block {
        return 0;
    }

    // Register as waiter so record_thread_exit wakes us. `stop_if_signalled`
    // wakes the same registration, and waking consumes it, so the loop
    // re-registers on every pass.
    let current_weak = current_thread_weak().unwrap();

    // Park until the target has exited, or — for an untraced wait — stopped.
    // thread_park_while may return spuriously (stale wake token, etc.), so
    // loop on the real condition.
    //
    // A kill also ends it, for the reason `WaitQueue::wait_until_killable`
    // documents: a child that never exits is a condition this thread cannot
    // bring about, so without the check a killed parent parks here forever and
    // survives even `SIGKILL`. A shell waiting on a job is the ordinary case.
    while !EXITED_THREADS.has_exited(target) && !has_stopped() && !current_thread_killed() {
        EXITED_THREADS.register_waiter(target, current_weak.clone());
        thread_park_while(|| {
            !EXITED_THREADS.has_exited(target) && !has_stopped() && !current_thread_killed()
        });
    }

    EXITED_THREADS.unregister_waiter(target);

    if current_thread_killed() {
        // The value never reaches userspace: this thread dies at the syscall
        // return boundary, which is the whole point of getting back to it.
        info.lock().errno = Errno::EINTR;
        return !0u64;
    }

    if !EXITED_THREADS.has_exited(target) {
        if !status_ptr.is_null() && !unsafe { try_write_user(status_ptr, STATUS_STOPPED) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        return pid;
    }

    // Now consume the exit code
    if let Some(code) = take_thread_exit_code(target) {
        if !status_ptr.is_null() && !unsafe { try_write_user(status_ptr, code) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        return pid;
    }

    // Should not happen - we were woken because it exited
    0
}

fn sys_sleep_ms(milliseconds: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let duration = Duration::from_millis(milliseconds);

    thread_sleep(duration);

    0
}

/// `reboot(cmd)`: stop the machine. Only returns on an unknown command, and
/// then with EINVAL — a successful call never comes back, so there is no
/// success value to report.
///
/// EDOS has no user ids, so there is no privilege to check: the guard against
/// a stray program stopping the machine is that `reboot` is not something a
/// program calls by accident.
fn sys_reboot(cmd: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    match cmd {
        REBOOT_POWER_OFF => power::power_off(),
        REBOOT_RESTART => power::reboot(),
        REBOOT_HALT => power::halt(),
        _ => {
            info.lock().errno = Errno::EINVAL;
            !0u64
        }
    }
}

/// POSIX `timespec`, as userspace lays it out for [`sys_nanosleep`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timespec {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

/// nanosleep(req, rem) -> 0 on success, -1 on error.
///
/// Sleeps until at least `req` has elapsed. A sleep ends on any wake, not only
/// on its deadline, so the remaining time is re-slept rather than reported: the
/// call returning is a promise the request was honoured.
///
/// `rem` is accepted for the POSIX signature and never written. A signal EDOS
/// delivers to a sleeping thread either is ignored or kills it, so the EINTR
/// case a caller would read the remainder for cannot happen.
fn sys_nanosleep(req_ptr: *const Timespec, _rem_ptr: *mut Timespec) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let Some(req) = (unsafe { try_read_user(req_ptr) }) else {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    };

    if req.tv_sec < 0 || !(0..1_000_000_000).contains(&req.tv_nsec) {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let deadline =
        crate::timer::Instant::now() + Duration::new(req.tv_sec as u64, req.tv_nsec as u32);

    while let Some(remaining) = deadline.checked_duration_since(crate::timer::Instant::now()) {
        if remaining.is_zero() {
            break;
        }
        thread_sleep(remaining);
        // A signal wakes the sleeper early, and this loop is what would put it
        // straight back to sleep: without suspending here, Ctrl+Z on a long
        // sleep would not take effect until the deadline passed. The thread
        // holds nothing at this point, which is what makes suspending it safe.
        // The deadline is absolute, so time spent suspended counts against it.
        stop_if_signalled();
        exit_if_killed();
    }

    0
}

fn sys_monotonic_time() -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    // HPET-driven uptime is monotonic with microsecond resolution.
    let micros = crate::timer::uptime_us();
    micros.saturating_mul(1_000)
}

fn sys_pipe(pipefd_ptr: *mut [u64; 2]) -> u64 {
    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    if pipefd_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    // Create new pipe
    let pipe = Arc::new(BlockingMutex::new(Pipe::new()));

    // The fd table is a `BlockingMutex` and its contended path parks, so the
    // `Arc` leaves the thread-info `IrqSpinlock` before it is locked: taken
    // inside that guard, the park would happen with interrupts disabled.
    let fd_table = info.lock().fd_table.clone();

    // Allocate read and write file descriptors
    let (read_fd, write_fd) = {
        let mut table = fd_table.lock();
        (
            table.allocate_fd(FileDescriptor::PipeRead(pipe.clone())),
            table.allocate_fd(FileDescriptor::PipeWrite(pipe)),
        )
    };

    // Copy file descriptor numbers to user space
    let pipefd = [read_fd, write_fd];
    let pipefd_bytes = core::mem::size_of_val(&pipefd);
    if !unsafe {
        try_copy_to_user(
            pipefd_ptr as *mut u8,
            pipefd.as_ptr() as *const u8,
            pipefd_bytes,
        )
    } {
        // Close both FDs to avoid leaking unreachable pipe ends
        {
            let mut table = fd_table.lock();
            table.close_fd(read_fd);
            table.close_fd(write_fd);
        }
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    0 // Success
}

fn sys_dup(oldfd: u64) -> u64 {
    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let mut table = fd_table.lock();

    let old_fd_descriptor = match table.get_fd(oldfd) {
        Some(fd) => fd.clone(),
        None => {
            drop(table);
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    old_fd_descriptor.inc_refcount();
    let nonblock = table.is_nonblock(oldfd);
    let new_fd = table.allocate_fd(old_fd_descriptor);
    // POSIX has both descriptors share one open file description and so one set
    // of status flags. Copying is as close as a table that has no such object
    // gets: the two agree until something calls F_SETFL on one of them.
    table.set_nonblock(new_fd, nonblock);
    new_fd
}

// fcntl commands (values match Linux).
const F_DUPFD: u64 = 0;
const F_GETFD: u64 = 1;
const F_SETFD: u64 = 2;
const F_GETFL: u64 = 3;
const F_SETFL: u64 = 4;
const F_DUPFD_CLOEXEC: u64 = 1030;

/// Only the close-on-exec flag exists.
const FD_CLOEXEC: u64 = 1;

/// `fcntl(fd, cmd, arg)`.
///
/// Supports the descriptor-flag, status-flag and duplication commands.
/// `O_NONBLOCK` is the only status flag that can be changed, which is all
/// POSIX.1-2024 requires of `F_SETFL` beyond `O_APPEND`; the access mode and
/// the creation flags are ignored there rather than refused, since a caller
/// that reads flags with `F_GETFL` and writes them back must not fail.
fn sys_fcntl(fd: u64, cmd: u64, arg: u64) -> i64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();
    let mut table = fd_table.lock();

    if table.get_fd(fd).is_none() {
        drop(table);
        info.lock().errno = Errno::EBADF;
        return -1;
    }

    match cmd {
        F_GETFD => {
            if table.is_cloexec(fd) {
                FD_CLOEXEC as i64
            } else {
                0
            }
        }
        F_SETFD => {
            table.set_cloexec(fd, arg & FD_CLOEXEC != 0);
            0
        }
        F_DUPFD | F_DUPFD_CLOEXEC => {
            let Some(desc) = table.get_fd(fd).cloned() else {
                drop(table);
                info.lock().errno = Errno::EBADF;
                return -1;
            };
            desc.inc_refcount();
            let nonblock = table.is_nonblock(fd);
            let new_fd = table.allocate_fd_from(desc, arg);
            table.set_cloexec(new_fd, cmd == F_DUPFD_CLOEXEC);
            table.set_nonblock(new_fd, nonblock);
            new_fd as i64
        }
        F_GETFL => {
            let Some(desc) = table.get_fd(fd) else {
                drop(table);
                info.lock().errno = Errno::EBADF;
                return -1;
            };
            let mut flags = descriptor_open_flags(desc);
            if table.is_nonblock(fd) {
                flags |= O_NONBLOCK;
            }
            flags as i64
        }
        F_SETFL => {
            table.set_nonblock(fd, arg & O_NONBLOCK != 0);
            0
        }
        _ => {
            drop(table);
            info.lock().errno = Errno::EINVAL;
            -1
        }
    }
}

fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    let fd_table = info.lock().fd_table.clone();

    // Get the file descriptor we want to duplicate
    let old_fd_descriptor = match fd_table.lock().get_fd(oldfd) {
        Some(fd) => fd.clone(),
        None => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Close the newfd if it's already in use (properly decrement refcounts)
    if let Some(old_desc) = fd_table.lock().close_fd(newfd) {
        close_fd_refcount(old_desc);
    }

    // Insert the duplicated descriptor at newfd, carrying the status flags
    // across for the reason `sys_dup` gives.
    old_fd_descriptor.inc_refcount();
    let mut table = fd_table.lock();
    let nonblock = table.is_nonblock(oldfd);
    table.insert_fd(newfd, old_fd_descriptor);
    table.set_nonblock(newfd, nonblock);

    newfd // Success - return the new fd number
}

/// Parse a null-terminated pointer array from userspace into a Vec of byte strings.
/// Returns Err with the appropriate errno on failure.
fn parse_user_string_array(
    ptr: *const *const u8,
    max_count: usize,
    max_item_len: usize,
    max_total: usize,
) -> Result<Vec<Vec<u8>>, Errno> {
    let mut storage: Vec<Vec<u8>> = Vec::new();

    if ptr.is_null() {
        return Ok(storage);
    }

    let mut total_bytes = 0usize;
    let mut terminated = false;

    for index in 0..max_count {
        let current_ptr = match unsafe { try_read_user(ptr.add(index)) } {
            Some(p) => p,
            None => return Err(Errno::EFAULT),
        };
        if current_ptr.is_null() {
            terminated = true;
            break;
        }

        let item = match copy_user_c_string(current_ptr, max_item_len) {
            Ok(bytes) => bytes,
            Err(UAccessError::Fault) => return Err(Errno::EFAULT),
            Err(UAccessError::TooLong) => return Err(Errno::EINVAL),
        };

        total_bytes += item.len() + 1;
        if total_bytes > max_total {
            return Err(Errno::EINVAL);
        }

        storage.push(item);
    }

    if !terminated {
        return Err(Errno::EINVAL);
    }

    Ok(storage)
}

/// Core spawn logic shared by sys_spawn and sys_spawn2.
///
/// `argv_storage` must already contain argv[0] as the first element (the resolved path).
/// `envp_storage` contains the environment strings (may be empty).
/// `depth` is the shebang recursion depth; pass 0 from callers. Shebang interpretation
/// is only performed at depth 0 to prevent infinite recursion.
// Every argument is a distinct field of the operation; grouping them into a
// struct would only move the same list one level out.
#[allow(clippy::too_many_arguments)]
fn do_spawn(
    path: &crate::fs::path::Path,
    path_str: &str,
    argv_storage: Vec<Vec<u8>>,
    envp_storage: Vec<Vec<u8>>,
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
    depth: u32,
) -> u64 {
    use crate::{fs::api as fs_api, thread::util::queue_spawn_thread};

    let spawn_start = crate::timer::Instant::now();
    let info = current_thread_info();

    // Save current cwd for child process
    let child_cwd = io::current_cwd(&info);

    x86_64::instructions::interrupts::enable();

    let finfo = match fs_api::file_info(path) {
        Ok(fi) => fi,
        Err(_) => {
            x86_64::instructions::interrupts::disable();
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Read up to 258 bytes to detect shebang or ELF magic without loading the
    // full binary. 258 covers the 4-byte ELF magic and a typical shebang line.
    let probe_len = core::cmp::min(finfo.size as usize, 258);
    let probe = match fs_api::read_bytes(path, 0, probe_len) {
        Ok(data) => data,
        Err(_) => {
            x86_64::instructions::interrupts::disable();
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Detect shebang (#!) at depth 0. Only recurse once to avoid infinite loops.
    if depth == 0 && probe.starts_with(b"#!") {
        // Parse the first line (up to newline or 256 bytes)
        let first_line_end = probe[2..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| p + 2)
            .unwrap_or_else(|| core::cmp::min(probe.len(), 258));
        let shebang_line = core::str::from_utf8(&probe[2..first_line_end])
            .unwrap_or("")
            .trim();

        // Split into interpreter path and optional argument
        let mut tokens = shebang_line.splitn(2, [' ', '\t']);
        let interp_path_str = match tokens.next() {
            Some(s) if !s.is_empty() => s.trim(),
            _ => {
                x86_64::instructions::interrupts::disable();
                info.lock().errno = Errno::ENOEXEC;
                return !0u64;
            }
        };
        let interp_arg: Option<&str> = tokens.next().map(|s| s.trim()).filter(|s| !s.is_empty());

        let resolve_path = |p: &str, cwd: &crate::fs::path::Path| {
            if p.starts_with('/') {
                crate::fs::path::Path::parse(p).map(|parsed| parsed.normalize())
            } else {
                Ok(cwd.join(p).normalize())
            }
        };

        let interp_path = match resolve_path(interp_path_str, &child_cwd) {
            Ok(p) => p,
            Err(_) => {
                x86_64::instructions::interrupts::disable();
                info.lock().errno = Errno::ENOEXEC;
                return !0u64;
            }
        };

        // Build new argv: [interpreter, optional_arg, script_path, original_args...]
        // argv_storage[0] is the resolved script path (argv[0]); original user args start at [1].
        let mut new_argv: Vec<Vec<u8>> = Vec::new();
        // argv[0] = interpreter path
        new_argv.push(format!("{interp_path}").as_bytes().to_vec());
        // optional interpreter argument
        if let Some(arg) = interp_arg {
            new_argv.push(arg.as_bytes().to_vec());
        }
        // script path (the original argv[0], i.e., the script being executed)
        new_argv.push(argv_storage[0].clone());
        // remaining original arguments (argv[1..])
        new_argv.extend_from_slice(&argv_storage[1..]);

        x86_64::instructions::interrupts::disable();

        return do_spawn(
            &interp_path,
            interp_path_str,
            new_argv,
            envp_storage,
            stdin_fd,
            stdout_fd,
            stderr_fd,
            1, // depth = 1: do not interpret shebang in the interpreter itself
        );
    }

    // Reject files that are neither ELF nor shebang
    if !probe.starts_with(b"\x7fELF") {
        x86_64::instructions::interrupts::disable();
        info.lock().errno = Errno::ENOEXEC;
        return !0u64;
    }

    // Resolve the inode for file-backed ELF loading. The inode Arc is held for
    // the duration of the mapping via each VmaBacking::FileBacked.
    let inode = match fs_api::resolve_inode(path) {
        Ok(ino) => ino,
        Err(_) => {
            x86_64::instructions::interrupts::disable();
            info.lock().errno = Errno::ENOEXEC;
            return !0u64;
        }
    };

    // Clone parent FD entries while interrupts are still enabled (BlockingMutex
    // requires interrupts for contention handling).
    // The status flags come across with each descriptor: a child handed a
    // non-blocking pipe end must find it behaving as its spawner's did.
    let parent_stdin = {
        let fd_table = info.lock().fd_table.clone();
        let fds = fd_table.lock();
        let carry = |fd: u64| {
            let (desc, nonblock) = fds.get_fd_nonblock(fd);
            desc.map(|desc| (desc, nonblock))
        };
        (carry(stdin_fd), carry(stdout_fd), carry(stderr_fd))
    };

    let argv_slices: Vec<&[u8]> = argv_storage.iter().map(|arg| arg.as_slice()).collect();
    let envp_slices: Vec<&[u8]> = envp_storage.iter().map(|e| e.as_slice()).collect();

    let user_thread = match Thread::new_user(
        inode,
        path,
        Some(path_str.to_string()),
        &argv_slices,
        &envp_slices,
        0,
        0,
        child_cwd,
    ) {
        Ok(thread) => thread,
        Err(e) => {
            log!("UserThread creation failed: {:?}", e);
            x86_64::instructions::interrupts::disable();
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Set up file descriptor redirections for stdin/stdout/stderr.
    // Uses parent FD entries cloned before interrupts were disabled.
    {
        let child_info = get_thread_info_by_id(user_thread.id).unwrap();
        let user_thread_info = child_info.lock();
        let (stdin_desc, stdout_desc, stderr_desc) = parent_stdin;

        for (fd, entry) in [(0, stdin_desc), (1, stdout_desc), (2, stderr_desc)] {
            let Some((desc, nonblock)) = entry else {
                continue;
            };
            desc.inc_refcount();
            let mut table = user_thread_info.fd_table.lock();
            table.insert_fd(fd, desc);
            table.set_nonblock(fd, nonblock);
        }
    }

    let child_pid = user_thread.id.0;

    // A child joins its spawner's process group, the way both fork paths
    // already hand theirs down. Without it every spawned process leads a group
    // of one, and a signal aimed at a group reaches only what was put there by
    // hand: `sshd` hanging up on a disconnected session would kill the shell
    // and leave the command it started running.
    //
    // Which group owns the terminal is not decided here. The kernel routes the
    // signal to whatever group `tcsetpgrp` named; picking that group is job
    // control, and job control is a shell's business.
    if let Some(parent) = current_thread() {
        user_thread.pgid.store(parent.pgid(), Ordering::Release);
    }

    let load_ns = spawn_start.elapsed().as_nanos() as u64;
    crate::log_debug!(
        "spawn: tid={} name={} load={}.{:03}ms",
        child_pid,
        path_str,
        load_ns / 1_000_000,
        (load_ns / 1_000) % 1_000
    );

    // The spawner owns the child's exit record: when it dies, the record has no
    // collector left and init inherits any child still running.
    if let Some(parent) = current_thread() {
        user_thread.parent.store(parent.id.0, Ordering::Release);
        user_thread
            .traced
            .store(trace::inherit_from(&parent), Ordering::Relaxed);
    }

    queue_spawn_thread(user_thread);

    x86_64::instructions::interrupts::disable();

    child_pid
}

fn sys_spawn(
    path_ptr: *const u8,
    argv_ptr: *const *const u8,
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> u64 {
    use crate::fs::path::Path;

    const MAX_PATH_LEN: usize = 1024;
    const MAX_ARGC: usize = 64;
    const MAX_ARG_LEN: usize = 4096;
    const MAX_ARG_TOTAL: usize = 16 * 1024;

    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    if path_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let path_bytes = match copy_user_c_string(path_ptr, MAX_PATH_LEN) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        Err(UAccessError::Fault) => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        Err(UAccessError::TooLong) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let path_str = match core::str::from_utf8(&path_bytes) {
        Ok(s) => s,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let resolve_path = |path_str: &str, cwd: &Path| -> Result<Path, crate::fs::path::ParseError> {
        if path_str.starts_with('/') {
            Path::parse(path_str).map(|p| p.normalize())
        } else {
            let joined = cwd.join(path_str);
            Ok(joined.normalize())
        }
    };

    let path = match resolve_path(path_str, &io::current_cwd(&info)) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let mut argv_storage: Vec<Vec<u8>> = Vec::new();
    argv_storage.push(format!("{path}").as_bytes().to_vec());

    match parse_user_string_array(argv_ptr, MAX_ARGC, MAX_ARG_LEN, MAX_ARG_TOTAL) {
        Ok(args) => argv_storage.extend(args),
        Err(errno) => {
            info.lock().errno = errno;
            return !0u64;
        }
    }

    // path_str lifetime ends with path_bytes; copy it as an owned string before calling do_spawn
    let path_str_owned = path_str.to_string();
    do_spawn(
        &path,
        &path_str_owned,
        argv_storage,
        Vec::new(),
        stdin_fd,
        stdout_fd,
        stderr_fd,
        0,
    )
}

fn sys_spawn2(args_ptr: *const SpawnArgs) -> u64 {
    use crate::fs::path::Path;

    const MAX_PATH_LEN: usize = 1024;
    const MAX_ARGC: usize = 64;
    const MAX_ARG_LEN: usize = 4096;
    const MAX_ARG_TOTAL: usize = 16 * 1024;
    const MAX_ENVC: usize = 256;
    const MAX_ENV_LEN: usize = 4096;
    const MAX_ENV_TOTAL: usize = 64 * 1024;

    let info = current_thread_info();

    info.lock().errno = Errno::Clear;

    if args_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let args: SpawnArgs = match unsafe { try_read_user(args_ptr) } {
        Some(a) => a,
        None => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };

    if args.path.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let path_bytes = match copy_user_c_string(args.path, MAX_PATH_LEN) {
        Ok(bytes) if !bytes.is_empty() => bytes,
        Ok(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
        Err(UAccessError::Fault) => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        Err(UAccessError::TooLong) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let path_str = match core::str::from_utf8(&path_bytes) {
        Ok(s) => s,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let resolve_path = |path_str: &str, cwd: &Path| -> Result<Path, crate::fs::path::ParseError> {
        if path_str.starts_with('/') {
            Path::parse(path_str).map(|p| p.normalize())
        } else {
            let joined = cwd.join(path_str);
            Ok(joined.normalize())
        }
    };

    let path = match resolve_path(path_str, &io::current_cwd(&info)) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let mut argv_storage: Vec<Vec<u8>> = Vec::new();
    argv_storage.push(format!("{path}").as_bytes().to_vec());

    match parse_user_string_array(args.argv, MAX_ARGC, MAX_ARG_LEN, MAX_ARG_TOTAL) {
        Ok(args_vec) => argv_storage.extend(args_vec),
        Err(errno) => {
            info.lock().errno = errno;
            return !0u64;
        }
    }

    let envp_storage =
        match parse_user_string_array(args.envp, MAX_ENVC, MAX_ENV_LEN, MAX_ENV_TOTAL) {
            Ok(env) => env,
            Err(errno) => {
                info.lock().errno = errno;
                return !0u64;
            }
        };

    let path_str_owned = path_str.to_string();
    do_spawn(
        &path,
        &path_str_owned,
        argv_storage,
        envp_storage,
        args.stdin_fd,
        args.stdout_fd,
        args.stderr_fd,
        0,
    )
}

/// `execve(path, argv, envp)`: replace this process's image, keeping its pid.
///
/// The sequence is dictated by one rule: nothing observable changes until the
/// point of no return, and after that point nothing may fail.
///
/// 1. Copy path/argv/envp out of user memory. Everything after this reads only
///    kernel memory, because the address space holding those strings is about
///    to disappear.
/// 2. Build the entire new image in a *fresh* address space, while the old one
///    is still live. A failure here returns an error with the process intact,
///    which is what POSIX requires of a failed exec.
/// 3. Terminate the sibling threads and wait for them to release the address
///    space. Failing this is the one case that can abandon a successful load.
/// 4. Point of no return: switch to the kernel page table, release the old
///    mappings, install the new ones, and rewrite the register context so the
///    syscall returns into the new image instead of the caller.
fn sys_execve(
    ctx: &mut SyscallContext,
    path_ptr: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> u64 {
    use crate::{
        fs::api as fs_api,
        memory::frame_allocator::frame_allocator,
        thread::{
            pipe::close_descriptor,
            thread::{load_process_image, quiesce_address_space},
        },
        util::per_cpu::write_fs_base,
    };

    const MAX_ARGC: usize = 64;
    const MAX_ARG_LEN: usize = 4096;
    const MAX_ARG_TOTAL: usize = 16 * 1024;
    const MAX_ENVC: usize = 128;
    const MAX_ENV_LEN: usize = 4096;
    const MAX_ENV_TOTAL: usize = 32 * 1024;

    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let Some(thread) = current_thread() else {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    };
    let Some(user_arc) = thread.user.clone() else {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    };

    x86_64::instructions::interrupts::enable();

    // --- 1. Everything the new image needs, copied out of the old one ---

    let path_bytes = match copy_user_c_string(path_ptr, MAX_PATH_LEN) {
        Ok(bytes) => bytes,
        Err(_) => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };
    let Ok(path_str) = core::str::from_utf8(&path_bytes) else {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    };
    let path = match io::resolve_path(path_str, &io::current_cwd(&info)) {
        Ok(p) => p,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let mut argv_storage: Vec<Vec<u8>> = Vec::new();
    argv_storage.push(format!("{path}").as_bytes().to_vec());
    match parse_user_string_array(argv, MAX_ARGC, MAX_ARG_LEN, MAX_ARG_TOTAL) {
        Ok(args) => argv_storage.extend(args),
        Err(errno) => {
            info.lock().errno = errno;
            return !0u64;
        }
    }
    let envp_storage = match parse_user_string_array(envp, MAX_ENVC, MAX_ENV_LEN, MAX_ENV_TOTAL) {
        Ok(env) => env,
        Err(errno) => {
            info.lock().errno = errno;
            return !0u64;
        }
    };

    let inode = match fs_api::resolve_inode(&path) {
        Ok(ino) => ino,
        Err(_) => {
            info.lock().errno = Errno::ENOENT;
            return !0u64;
        }
    };

    // --- 2. Build the new image beside the old one ---

    let argv_slices: Vec<&[u8]> = argv_storage.iter().map(|a| a.as_slice()).collect();
    let envp_slices: Vec<&[u8]> = envp_storage.iter().map(|e| e.as_slice()).collect();

    let image = match load_process_image(&inode, &path, &argv_slices, &envp_slices) {
        Ok(image) => image,
        Err(e) => {
            log!("execve: loading {path} failed: {e:?}");
            info.lock().errno = Errno::ENOEXEC;
            return !0u64;
        }
    };

    // --- 3. The old address space must belong to this thread alone ---

    if !quiesce_address_space(&thread) {
        // The load succeeded but a sibling will not stop. Give the new image
        // back rather than unmap an address space someone may still be running.
        release_user_mappings_of_image(image);
        info.lock().errno = Errno::EAGAIN;
        return !0u64;
    }

    // --- 4. Point of no return ---

    // Descriptors marked close-on-exec go before the swap, while user memory is
    // still addressable for anything their teardown needs.
    let cloexec = {
        let fd_table = info.lock().fd_table.clone();

        fd_table.lock().take_cloexec()
    };
    let pid = info.lock().pid;
    for (_fd, desc) in cloexec {
        close_descriptor(desc, pid);
    }

    // Detach the outgoing address space and attach the new one in a single
    // step, then start running on it. Only after that is the old one taken
    // apart: `context_switch_to` reloads CR3 from `user.cr3` on every switch,
    // so freeing a page table that is still published there would hand a
    // preemption a dangling CR3.
    let (parts, old) = install_image(&thread, &user_arc, &info, image);

    unsafe { Cr3::write(parts.new_cr3.0, parts.new_cr3.1) };

    let DetachedAddressSpace {
        mut memory_manager,
        vmas,
        pml4,
        stack_top,
    } = old;
    crate::thread::thread::flush_shared_mappings(&vmas);
    crate::thread::thread::release_mappings(&mut memory_manager, &vmas, pid, stack_top);
    drop(memory_manager);
    unsafe { frame_allocator().deallocate_frame(pml4) };

    let LoadedImageParts {
        entry_point,
        user_stack_pointer,
        argc,
        argv_ptr,
        envp_ptr,
        tls_fs_base,
        new_cr3: _,
    } = parts;

    thread.tls_base.store(tls_fs_base, Ordering::Release);
    write_fs_base(VirtAddr::new(tls_fs_base));
    thread.signal.reset_for_exec();

    // Return into the new image: the syscall stub restores every register from
    // this context and sysrets to `rip` with `rsp`, so rewriting it here is the
    // whole of "start executing the new program".
    *ctx = SyscallContext {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        rbp: 0,
        rbx: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rdx: envp_ptr,
        rsi: argv_ptr,
        rdi: argc as u64,
        rax: 0,
        rip: entry_point.as_u64(),
        rflags: RFlags::INTERRUPT_FLAG.bits(),
        rsp: user_stack_pointer,
    };

    0
}

struct LoadedImageParts {
    entry_point: VirtAddr,
    user_stack_pointer: u64,
    argc: usize,
    argv_ptr: u64,
    envp_ptr: u64,
    tls_fs_base: u64,
    new_cr3: (x86_64::structures::paging::PhysFrame, Cr3Flags),
}

/// Move a freshly loaded image into the process, keeping every `Arc` identity.
///
/// The `MemoryManager` and `VmaSet` allocations are replaced *by content* rather
/// than by pointer: `UserThreadInfo`, the `MemoryManager`'s own back-reference
/// and any in-flight fault handler all hold clones of those `Arc`s, and swapping
/// the pointers would leave them addressing the image that just died.
/// The address space `execve` replaced, detached from the process and owned by
/// the caller, which must tear it down.
struct DetachedAddressSpace {
    memory_manager: crate::memory::mapper::MemoryManager,
    vmas: VmaSet,
    pml4: x86_64::structures::paging::PhysFrame,
    stack_top: u64,
}

fn install_image(
    thread: &Thread,
    user_arc: &Arc<RwLock<crate::thread::UserThread>>,
    info: &Arc<IrqSpinlock<UserThreadInfo>>,
    image: crate::thread::thread::LoadedImage,
) -> (LoadedImageParts, DetachedAddressSpace) {
    let kernel_pml4_flags = crate::boot::boot_info().cr3.1;

    let mut user = user_arc.write();

    let mut new_mm = image.memory_manager;
    // Take the VMAs out of the image's temporary Arc and into the one the
    // process already publishes.
    let new_vmas = core::mem::replace(&mut *image.vma_set.lock(), VmaSet::new());
    new_mm.vmas = Some(user.vmas.clone());
    let old_vmas = {
        let mut vmas = ranked_lock!(RANK_VMAS, "exec::vmas", user.vmas);
        core::mem::replace(&mut **vmas, new_vmas)
    };
    let old_mm = {
        let mut mm = ranked_lock!(RANK_USER_MM, "exec::mm", user.memory_manager);
        core::mem::replace(&mut **mm, new_mm)
    };
    let old_pml4 = user.cr3.0;
    let old_stack_top = user.process_stack_top.load(Ordering::Acquire);

    // Both halves under the one write guard: a preemption between them would
    // resume this thread on whichever half was still stale.
    thread.set_user_cr3(&mut user, (image.pml4_frame, kernel_pml4_flags));
    user.tls = image.tls;
    user.heap_break = image.heap_break;
    user.cmdline = image.cmdline;
    user.process_stack_top
        .store(image.stack_top, Ordering::Release);
    // A fresh image has one thread again, so TLS slots restart above slot 0.
    user.next_tls_slot.store(1, Ordering::Release);

    info.lock()
        .next_mmap_addr
        .store(image.heap_break, Ordering::Release);

    // Register the process as a mapper of the new file-backed VMAs so a later
    // truncate can unmap them here. Collected under the VmaSet lock and
    // registered outside it: inode.mappers outranks vmas.
    let file_backed: Vec<Arc<crate::fs::inode::VfsInode>> = {
        let vmas = ranked_lock!(RANK_VMAS, "exec::vmas", user.vmas);
        vmas.iter()
            .filter_map(|vma| match &vma.backing {
                VmaBacking::FileBacked { inode, .. } => Some(Arc::clone(inode)),
                _ => None,
            })
            .collect()
    };
    let weak = Arc::downgrade(user_arc);
    for inode in file_backed {
        ranked_lock!(RANK_MAPPERS, "inode.mappers", inode.mappers).push(weak.clone());
    }

    (
        LoadedImageParts {
            entry_point: image.entry_point,
            user_stack_pointer: image.user_stack_pointer,
            argc: image.argc,
            argv_ptr: image.argv_ptr,
            envp_ptr: image.envp_ptr,
            tls_fs_base: image.tls_fs_base,
            new_cr3: (image.pml4_frame, kernel_pml4_flags),
        },
        DetachedAddressSpace {
            memory_manager: old_mm,
            vmas: old_vmas,
            pml4: old_pml4,
            stack_top: old_stack_top,
        },
    )
}

/// Discard an image that was built but never installed.
///
/// Only the page tables and the frames the loader mapped exist yet; nothing
/// else in the kernel refers to them.
fn release_user_mappings_of_image(mut image: crate::thread::thread::LoadedImage) {
    use crate::memory::frame_allocator::frame_allocator;

    image.memory_manager.clean_lower_half();
    unsafe { frame_allocator().deallocate_frame(image.pml4_frame) };
}

fn sys_clone(
    parent_ctx: &mut SyscallContext,
    func_ptr: u64,
    arg: u64,
    _flags: u64,
    child_stack: u64,
) -> u64 {
    let parent_thread = match current_thread() {
        Some(t) => t,
        None => {
            current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let parent_user = match &parent_thread.user {
        Some(u) => u,
        None => {
            current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Allocate user stack if not provided. `claimed_stack` carries the range this
    // call owns, so a later failure can hand it back.
    let (user_stack_top, claimed_stack) = if child_stack == 0 {
        // Allocate a new user stack using internal mmap
        let parent_info = current_thread_info();
        let stack_size = 2 * 1024 * 1024u64; // 2MB stack

        // Claim the range before mapping it. Threads of one process share an
        // address space, so two concurrent spawns searching for a free range
        // without claiming it would both land on the same stack.
        let Some(stack_bottom) = crate::syscalls::memory::claim_range(
            parent_user,
            &parent_info,
            0,
            stack_size,
            VmaProt::READ | VmaProt::WRITE,
            VmaFlags::PRIVATE | VmaFlags::GROWSDOWN,
            VmaBacking::Stack,
        ) else {
            return !0u64;
        };

        // Map the stack
        let page_flags =
            PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;

        if parent_info
            .lock()
            .memory_manager
            .lock()
            .map_memory(stack_bottom, stack_size, page_flags)
            .is_err()
        {
            let user_read = parent_user.read();
            ranked_lock!(RANK_VMAS, "sys_clone::stack_unclaim", user_read.vmas)
                .remove(&stack_bottom);
            parent_info.lock().errno = Errno::ENOMEM;
            return !0u64;
        }

        let top = (stack_bottom.as_u64() + stack_size) & !(STACK_ALIGNMENT - 1);
        (top, Some((stack_bottom, stack_size)))
    } else {
        (child_stack, None)
    };

    // Allocate kernel stack for child
    let kernel_stack_top = kthread_stack_alloc();

    // Get parent UserThread data
    let parent_user_read = parent_user.read();
    let cr3 = parent_user_read.cr3;
    let memory_manager = parent_user_read.memory_manager.clone();
    let parent_heap_break = parent_user_read.heap_break;
    let parent_cmdline = parent_user_read.cmdline.clone();
    let parent_vmas = parent_user_read.vmas.clone();
    let process_stack_top = parent_user_read.process_stack_top.clone();
    let address_space_refs = parent_user_read.address_space_refs.clone();
    let mut tls_template = parent_user_read
        .tls
        .as_ref()
        .map(|tls| tls.template.clone());
    let next_tls_slot = parent_user_read.next_tls_slot.clone();

    // Create child context - clone parent's CPU state
    let mut child_ctx = CpuContext::new_user_thread(func_ptr, user_stack_top - 8);

    // Copy callee-saved registers from parent
    child_ctx.r15 = parent_ctx.r15;
    child_ctx.r14 = parent_ctx.r14;
    child_ctx.r13 = parent_ctx.r13;
    child_ctx.r12 = parent_ctx.r12;
    child_ctx.rbp = parent_ctx.rbp;
    child_ctx.rbx = parent_ctx.rbx;

    // Set child-specific values
    child_ctx.rax = 0; // Child returns 0
    child_ctx.rdi = arg; // First argument to function
    child_ctx.rsi = 0;

    drop(parent_user_read);

    // Allocate new thread ID
    let child_id = allocate_thread_id();

    // Every thread gets a control block, whatever the image declared; the
    // parent's template is empty when it had no `PT_TLS`.
    let template = tls_template
        .take()
        .unwrap_or_else(|| Arc::new(TlsTemplate::empty()));
    let allocation = {
        let mut manager_guard = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
        let tls_slot = next_tls_slot.fetch_add(1, Ordering::Relaxed);
        match crate::thread::thread::allocate_tls_region(&template, tls_slot, &mut manager_guard) {
            Ok(allocation) => allocation,
            Err(_) => {
                // Unwind the stack claimed above, which this path used to leave
                // mapped because the VMA had not been recorded yet.
                if let Some((stack_bottom, stack_size)) = claimed_stack {
                    let _ = manager_guard.unmap_memory(stack_bottom, stack_size);
                    drop(manager_guard);
                    let user_read = parent_user.read();
                    ranked_lock!(RANK_VMAS, "sys_clone::stack_unclaim", user_read.vmas)
                        .remove(&stack_bottom);
                } else {
                    drop(manager_guard);
                }
                kthread_stack_free(kernel_stack_top);
                current_thread_info().lock().errno = Errno::ENOMEM;
                return !0u64;
            }
        }
    };
    let tls_fs_base = allocation.fs_base;
    let tls_runtime = Some(allocation.runtime);

    // Add the new TLS VMA to the shared VmaSet; the stack was claimed above.
    ranked_lock!(RANK_VMAS, "sys_clone::tls_vma_insert", parent_vmas)
        .insert_validated(allocation.vma);

    address_space_refs.fetch_add(1, Ordering::AcqRel);

    let child_user_cr3 = Thread::encode_cr3(cr3);
    let child_user = Arc::new(RwLock::new(crate::thread::UserThread {
        pid: child_id.0,
        cr3,
        memory_manager: memory_manager.clone(),
        vmas: parent_vmas, // Arc clone - shared address space
        tls: tls_runtime,
        heap_break: parent_heap_break,
        cmdline: parent_cmdline,
        address_space_refs,
        process_stack_top,
        next_tls_slot, // Arc clone - shared counter
    }));

    // Create child Thread
    let child_thread = Arc::new(Thread {
        id: child_id,
        kstack_top: kernel_stack_top,
        ctx: Mutex::new(child_ctx),
        state: AtomicU8::new(State::Ready as u8),
        name: Arc::new(format!("{}-thread-{}", parent_thread.name, child_id.0)),
        cpu_affinity: AtomicU32::new(0),
        flags: AtomicU32::new(0),
        slice_deadline: AtomicU64::new(0),
        vruntime: AtomicU64::new(0),
        vdeadline: AtomicU64::new(0),
        vlag: AtomicI64::new(0),
        slice_ns: AtomicU64::new(parent_thread.slice_ns.load(Ordering::Acquire)),
        priority: AtomicU8::new(parent_thread.priority()),
        // A loan belongs to the lock the parent holds, not to the child,
        // which starts holding nothing.
        lent_priority: AtomicU8::new(0),
        parent: AtomicU64::new(parent_thread.id.0),
        sleep_deadline: AtomicU64::new(0),
        cpu_time_ns: AtomicU64::new(0),
        run_start_ns: AtomicU64::new(0),
        created_at_ns: AtomicU64::new(crate::timer::Instant::now().as_nanos()),
        demand_faults: AtomicU32::new(0),
        tls_base: AtomicU64::new(tls_fs_base),
        cpu: AtomicU32::new(0),
        exit_code: AtomicI32::new(0),
        killed: AtomicBool::new(false),
        stop_requested: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        wake_pending: AtomicBool::new(false),
        last_syscall: AtomicU32::new(crate::thread::thread::NO_SYSCALL),
        traced: AtomicU64::new(trace::inherit_from(&parent_thread)),
        pgid: AtomicU64::new(parent_thread.pgid()),
        signal: SignalState::new(),
        user: Some(child_user),
        user_cr3: AtomicU64::new(child_user_cr3),
        rq_link: Link::new(),
        context_saved: AtomicBool::new(true),
        fpu: core::cell::UnsafeCell::new(crate::drivers::fpu::FpuState::default()),
        fpu_init: AtomicBool::new(false),
        fpu_cpu: AtomicU32::new(u32::MAX),
        owned_ops: crate::thread::irqlock::IrqSpinlock::new(HeaplessVec::new()),
        #[cfg(debug_assertions)]
        lock_ranks: core::cell::UnsafeCell::new(heapless::Vec::new()),
        #[cfg(debug_assertions)]
        borrowed_dma: AtomicU32::new(0),
    });

    // Clone parent's UserThreadInfo - share fd_table
    let parent_info = current_thread_info();
    let parent_info_guard = parent_info.lock();

    insert_thread(child_thread.clone());
    insert_thread_info(
        child_id,
        Arc::new(IrqSpinlock::new(UserThreadInfo {
            pid: child_id.0,
            errno: Errno::Clear,
            fd_table: parent_info_guard.fd_table.clone(), // Arc clone - shared
            next_mmap_addr: parent_info_guard.next_mmap_addr.clone(), // Arc clone - shared
            memory_manager,
            cwd: parent_info_guard.cwd.clone(), // Arc clone - shared
            user_id: parent_info_guard.user_id,
            group_id: parent_info_guard.group_id,
        })),
    );

    drop(parent_info_guard);

    // Queue the child thread
    crate::thread::util::queue_spawn_thread(child_thread);

    // Parent returns child thread ID
    // A thread of a shell process is part of that shell: init spawns a
    // supervisor thread per service and grants from there, and a compositor is
    // free to do its work off its main thread. The privilege is per thread id
    // because that is what this kernel has -- there is no thread-group id yet,
    // so `pid` is a thread's own id and a process is only a parent relation.
    if crate::window::shell::is_shell(current_thread_info().lock().pid) {
        crate::window::shell::grant(child_id.0);
    }

    child_id.0
}

fn sys_fork(parent_ctx: &mut SyscallContext) -> i64 {
    use core::sync::atomic::AtomicUsize;
    use x86_64::structures::paging::OffsetPageTable;

    use crate::memory::{
        cow::clone_user_page_tables_cow,
        mapper::{MemoryManager, get_level_4_table},
        shared::SharedMemory,
    };

    let parent_thread = match current_thread() {
        Some(t) => t,
        None => {
            current_thread_info().lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    let parent_user = match &parent_thread.user {
        Some(u) => u,
        None => {
            current_thread_info().lock().errno = Errno::EINVAL;
            return -1;
        }
    };

    // Read parent address-space data before switching page tables.
    let parent_user_read = parent_user.read();
    let parent_cr3 = parent_user_read.cr3;
    let parent_heap_break = parent_user_read.heap_break;
    let parent_cmdline = parent_user_read.cmdline.clone();
    let parent_process_stack_top = parent_user_read.process_stack_top.load(Ordering::Acquire);
    let parent_tls = parent_user_read.tls.clone();
    let parent_fs_base = parent_thread.tls_base.load(Ordering::Acquire);

    // Clone COW page tables using the parent's VmaSet.
    // Must be called with parent's CR3 active.
    // tlb_shootdown_all() inside flushes all CPUs' stale writable entries.
    // This runs before anything else the child owns is built, so a clone that
    // runs out of frames has nothing but its own partial tree to give back.
    let child_pml4_frame = {
        let parent_vmas = ranked_lock!(RANK_VMAS, "user.vmas", parent_user_read.vmas);
        match unsafe { clone_user_page_tables_cow(parent_cr3.0, &parent_vmas) } {
            Some(frame) => frame,
            None => {
                drop(parent_vmas);
                drop(parent_user_read);
                current_thread_info().lock().errno = Errno::ENOMEM;
                return -1;
            }
        }
    };

    // Deep-clone the VmaSet: each VMA is cloned, SHM entries get inc_ref.
    // FileBacked VMAs get a fresh empty pages vec — the child re-faults lazily.
    let child_vma_set = {
        let parent_vmas = ranked_lock!(RANK_VMAS, "user.vmas", parent_user_read.vmas);
        let mut cloned = VmaSet::new();
        for vma in parent_vmas.iter() {
            match &vma.backing {
                VmaBacking::SharedMemory { shm_id } => {
                    if let Some(shm) = SharedMemory::get(*shm_id) {
                        let _ = shm.inc_ref();
                    }
                    cloned.insert_validated(vma.clone());
                }
                VmaBacking::FileBacked {
                    inode,
                    file_offset,
                    shared,
                    writable_mapping,
                    pages,
                } => {
                    // Child gets a fresh pages vec (no shared Arc<CachedPage> refs).
                    // Child will re-fault and fill its own page slots lazily.
                    let num_pages = pages.len();
                    cloned.insert_validated(Vma {
                        start: vma.start,
                        end: vma.end,
                        prot: vma.prot,
                        flags: vma.flags,
                        backing: VmaBacking::FileBacked {
                            inode: Arc::clone(inode),
                            file_offset: *file_offset,
                            shared: *shared,
                            writable_mapping: *writable_mapping,
                            pages: alloc::vec![None; num_pages],
                        },
                    });
                }
                _ => {
                    cloned.insert_validated(vma.clone());
                }
            }
        }
        cloned
    };

    drop(parent_user_read);

    // Read parent info
    let parent_info = current_thread_info();
    let parent_next_mmap = {
        let guard = parent_info.lock();
        guard.next_mmap_addr.load(Ordering::Acquire)
    };
    let parent_user_id;
    let parent_group_id;
    let parent_cwd_clone;
    {
        let guard = parent_info.lock();
        parent_user_id = guard.user_id;
        parent_group_id = guard.group_id;
        parent_cwd_clone = guard.cwd.lock().clone();
    }

    // Deep-clone the fd_table: new table with cloned entries and their flags,
    // refcounts bumped.
    let child_fd_table = {
        let guard = parent_info.lock();
        let parent_fds = guard.fd_table.lock();
        Arc::new(BlockingMutex::new(parent_fds.deep_clone()))
    };

    // Switch to kernel page table for the remaining setup.
    switch_to_kernel_page();

    let phys_offset = crate::boot::boot_info().physical_memory_offset;

    // Wrap the deep-cloned VmaSet in an Arc so it can be shared between
    // the child MemoryManager and UserThread.
    let child_vma_set_arc = Arc::new(PreemptSpinlock::new(child_vma_set));

    // Build a MemoryManager for the child via HHDM (no CR3 switch needed).
    // Explicitly clone reloc_table (Arc clone, cheap) and reloc_vma_range from
    // the parent so the child can apply lazy relocs after COW faults.
    let child_mm = {
        let child_page_table = unsafe { get_level_4_table((child_pml4_frame, parent_cr3.1)) };
        let table = unsafe { OffsetPageTable::new(child_page_table, phys_offset) };
        let mut mm = MemoryManager::new(table);
        mm.pml4_frame = Some(child_pml4_frame);
        mm.vmas = Some(child_vma_set_arc.clone());
        {
            let parent_user_guard = parent_user.read();
            let parent_mm = ranked_lock!(RANK_USER_MM, "user.mm", parent_user_guard.memory_manager);
            mm.reloc_table = parent_mm.reloc_table.clone();
            mm.reloc_vma_range = parent_mm.reloc_vma_range.clone();
            mm.load_base = parent_mm.load_base;
        }
        Arc::new(PreemptSpinlock::new(mm))
    };

    // Allocate kernel stack for the child.
    let kernel_stack_top = kthread_stack_alloc();

    // Set up child CPU context: resume at the same userspace RIP with rax=0.
    // new_user_thread uses INTERRUPT_FLAG for rflags; we override with the
    // parent's saved user RFLAGS (parent_ctx.rflags is R11 = user RFLAGS at
    // syscall entry), ensuring the IF bit stays set.
    let mut child_ctx = CpuContext::new_user_thread(parent_ctx.rip, parent_ctx.rsp);
    child_ctx.interrupt_stack_frame.cpu_flags =
        RFlags::from_bits_truncate(parent_ctx.rflags) | RFlags::INTERRUPT_FLAG;
    child_ctx.rax = 0; // fork returns 0 in child

    // Every register the SYSCALL stub hands back to the caller, because the
    // child returns from the same instruction and the convention is the same
    // one on both sides. That covers the argument registers as well as the
    // callee-saved set: the stub restores them, so a caller may keep a live
    // value in one across the `syscall`, and the raw stubs declare only rax,
    // rcx and r11 as clobbered — which is what tells the compiler it may.
    // Dropping any of them here hands the child a zero where its parent has a
    // pointer or a length, at whichever call site the register allocator
    // chose, and nothing at that call site says so. RCX and R11 are excluded
    // because SYSCALL itself consumes them for the return address and RFLAGS.
    child_ctx.rdi = parent_ctx.rdi;
    child_ctx.rsi = parent_ctx.rsi;
    child_ctx.rdx = parent_ctx.rdx;
    child_ctx.r8 = parent_ctx.r8;
    child_ctx.r9 = parent_ctx.r9;
    child_ctx.r10 = parent_ctx.r10;
    child_ctx.rbx = parent_ctx.rbx;
    child_ctx.rbp = parent_ctx.rbp;
    child_ctx.r12 = parent_ctx.r12;
    child_ctx.r13 = parent_ctx.r13;
    child_ctx.r14 = parent_ctx.r14;
    child_ctx.r15 = parent_ctx.r15;

    let child_id = allocate_thread_id();

    let child_user_cr3 = Thread::encode_cr3((child_pml4_frame, parent_cr3.1));
    let child_user_arc = Arc::new(RwLock::new(crate::thread::UserThread {
        pid: child_id.0,
        cr3: (child_pml4_frame, parent_cr3.1),
        memory_manager: child_mm.clone(),
        vmas: child_vma_set_arc,
        tls: parent_tls,
        heap_break: parent_heap_break,
        cmdline: parent_cmdline,
        address_space_refs: Arc::new(AtomicUsize::new(1)),
        process_stack_top: Arc::new(AtomicU64::new(parent_process_stack_top)),
        next_tls_slot: Arc::new(AtomicU64::new(1)), // fresh counter, slot 0 inherited via COW
    }));

    let child_thread = Arc::new(Thread {
        id: child_id,
        kstack_top: kernel_stack_top,
        ctx: Mutex::new(child_ctx),
        state: AtomicU8::new(State::Ready as u8),
        name: Arc::new(format!("{}-fork-{}", parent_thread.name, child_id.0)),
        cpu_affinity: AtomicU32::new(0),
        flags: AtomicU32::new(0),
        slice_deadline: AtomicU64::new(0),
        vruntime: AtomicU64::new(0),
        vdeadline: AtomicU64::new(0),
        vlag: AtomicI64::new(0),
        slice_ns: AtomicU64::new(parent_thread.slice_ns.load(Ordering::Acquire)),
        priority: AtomicU8::new(parent_thread.priority()),
        // A loan belongs to the lock the parent holds, not to the child,
        // which starts holding nothing.
        lent_priority: AtomicU8::new(0),
        parent: AtomicU64::new(parent_thread.id.0),
        sleep_deadline: AtomicU64::new(0),
        cpu_time_ns: AtomicU64::new(0),
        run_start_ns: AtomicU64::new(0),
        created_at_ns: AtomicU64::new(crate::timer::Instant::now().as_nanos()),
        demand_faults: AtomicU32::new(0),
        tls_base: AtomicU64::new(parent_fs_base),
        cpu: AtomicU32::new(0),
        exit_code: AtomicI32::new(0),
        killed: AtomicBool::new(false),
        stop_requested: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        wake_pending: AtomicBool::new(false),
        last_syscall: AtomicU32::new(crate::thread::thread::NO_SYSCALL),
        traced: AtomicU64::new(trace::inherit_from(&parent_thread)),
        pgid: AtomicU64::new(parent_thread.pgid()),
        signal: SignalState::new(),
        user: Some(child_user_arc.clone()),
        user_cr3: AtomicU64::new(child_user_cr3),
        rq_link: Link::new(),
        context_saved: AtomicBool::new(true),
        fpu: {
            // Save parent's current FPU/SSE state and copy to child.
            let mut fpu_state = crate::drivers::fpu::FpuState::default();
            unsafe { crate::drivers::fpu::save_fpu_state(&mut fpu_state) };
            core::cell::UnsafeCell::new(fpu_state)
        },
        fpu_init: AtomicBool::new(true),
        fpu_cpu: AtomicU32::new(u32::MAX),
        owned_ops: crate::thread::irqlock::IrqSpinlock::new(HeaplessVec::new()),
        #[cfg(debug_assertions)]
        lock_ranks: core::cell::UnsafeCell::new(heapless::Vec::new()),
        #[cfg(debug_assertions)]
        borrowed_dma: AtomicU32::new(0),
    });

    insert_thread(child_thread.clone());
    insert_thread_info(
        child_id,
        Arc::new(IrqSpinlock::new(UserThreadInfo {
            pid: child_id.0,
            errno: Errno::Clear,
            fd_table: child_fd_table,
            next_mmap_addr: Arc::new(AtomicU64::new(parent_next_mmap)),
            memory_manager: child_mm,
            cwd: Arc::new(BlockingMutex::new(parent_cwd_clone)),
            user_id: parent_user_id,
            group_id: parent_group_id,
        })),
    );

    // Task 3.4b: Register the fork child as a mapper of every FileBacked VMA it
    // inherited so that truncate/invalidate_mappings_above can reach the child.
    // Collect inode Arcs under the VmaSet lock, then register outside the lock
    // to avoid holding two locks simultaneously (inode.mappers > vmas ordering).
    {
        let file_backed_inodes: Vec<Arc<crate::fs::inode::VfsInode>> = {
            let child_user_read = child_user_arc.read();
            let vmas = ranked_lock!(RANK_VMAS, "user.vmas", child_user_read.vmas);
            vmas.iter()
                .filter_map(|vma| {
                    if let VmaBacking::FileBacked { inode, .. } = &vma.backing {
                        Some(Arc::clone(inode))
                    } else {
                        None
                    }
                })
                .collect()
        };
        let weak = Arc::downgrade(&child_user_arc);
        for inode in file_backed_inodes {
            ranked_lock!(RANK_MAPPERS, "inode.mappers", inode.mappers).push(weak.clone());
        }
    }

    crate::thread::util::queue_spawn_thread(child_thread);

    // Restore parent's address space before returning to userspace.
    unsafe { Cr3::write(parent_cr3.0, parent_cr3.1) };

    child_id.0 as i64
}

/// Longest path a syscall accepts, NUL excluded.
pub const MAX_PATH_LEN: usize = 1024;

/// Caller-owned scratch space for [`copy_user_path`].
pub type PathBuf = [u8; MAX_PATH_LEN];

/// Copy a NUL-terminated path out of user memory and validate it as UTF-8.
///
/// The buffer belongs to the caller and is meant to be a stack array: a path is
/// bounded by `MAX_PATH_LEN`, so `open`, `stat`, `mkdir`, `unlink`, `rename`
/// and friends have no reason to reach the allocator on every call.
pub fn copy_user_path(buf: &mut PathBuf, ptr: *const u8) -> Result<&str, Errno> {
    if ptr.is_null() {
        return Err(Errno::EFAULT);
    }

    let len = match unsafe { try_copy_string_from_user(buf.as_mut_ptr(), ptr, MAX_PATH_LEN) } {
        Ok(len) => len,
        Err(UAccessError::TooLong) => return Err(Errno::EINVAL),
        Err(UAccessError::Fault) => return Err(Errno::EFAULT),
    };

    if len == 0 {
        return Err(Errno::EINVAL);
    }

    core::str::from_utf8(&buf[..len]).map_err(|_| Errno::EINVAL)
}

fn copy_user_c_string(ptr: *const u8, max_len: usize) -> Result<Vec<u8>, UAccessError> {
    if ptr.is_null() {
        return Err(UAccessError::Fault);
    }

    let mut buf = vec![0u8; max_len];
    let len = unsafe { try_copy_string_from_user(buf.as_mut_ptr(), ptr, max_len) }?;

    buf.truncate(len);
    Ok(buf)
}

/// SYS_PING: send an ICMP echo request and wait for the reply.
///
/// Arguments:
///   - rdi: pointer to [u8; 4] destination IP address in userspace
///   - rsi: ICMP identifier (u16)
///   - rdx: ICMP sequence number (u16)
///   - r10: timeout in milliseconds (u64); 0 uses a default of 5000 ms
///
/// Returns RTT in microseconds on success, or u64::MAX on timeout / error.
fn sys_ping(dst_ip_ptr: *const [u8; 4], id: u16, seq: u16, timeout_ms: u64) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if dst_ip_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let dst_ip = match unsafe { try_read_user(dst_ip_ptr) } {
        Some(ip) => ip,
        None => {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
    };

    // Check that the net stack is initialized.
    if crate::net::stack::NET_STACK.get().is_none() {
        info.lock().errno = Errno::EINVAL;
        return !0u64;
    }

    let timeout_ms = if timeout_ms == 0 { 5000 } else { timeout_ms };
    let timeout = Duration::from_millis(timeout_ms);

    crate::net::stack::syscall_ping(dst_ip, id, seq, timeout).unwrap_or(!0u64)
}

/// SYS_NETINFO: write network interface information as text into a user buffer.
///
/// Arguments:
///   - rdi: pointer to user buffer
///   - rsi: buffer length in bytes
///
/// Returns the number of bytes written on success, or u64::MAX on error.
fn sys_netinfo(buf_ptr: *mut u8, buf_len: usize) -> u64 {
    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    if buf_ptr.is_null() || buf_len == 0 || !access_ok(buf_ptr as u64, buf_len) {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let text = {
        use alloc::fmt::Write;
        let mut out = alloc::string::String::with_capacity(256);

        // ANSI: \x1b[1m = bold, \x1b[32m = green, \x1b[31m = red,
        //       \x1b[36m = cyan, \x1b[0m = reset

        // lo - loopback
        let _ = writeln!(out, "1: \x1b[1mlo\x1b[0m: <LOOPBACK,\x1b[32mUP\x1b[0m>");
        let _ = writeln!(out, "    inet \x1b[36m127.0.0.1/8\x1b[0m");

        // eth0 - e1000e
        if let Some(stack) = crate::net::stack::NET_STACK.get() {
            let s = stack.lock();
            let mac = s.mac();
            let link = s.nic.link_up();
            let prefix = s.subnet_mask.iter().map(|b| b.count_ones()).sum::<u32>();
            let (flags, state_color) = if link {
                ("UP,LOWER_UP", "\x1b[32m") // green
            } else {
                ("NO-CARRIER", "\x1b[31m") // red
            };
            let _ = write!(
                out,
                "2: \x1b[1meth0\x1b[0m: <BROADCAST,MULTICAST,{}{}\x1b[0m>\n    link/ether {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}\n    inet \x1b[36m{}.{}.{}.{}/{}\x1b[0m\n    gateway {}.{}.{}.{}\n",
                state_color,
                flags,
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5],
                s.local_ip[0],
                s.local_ip[1],
                s.local_ip[2],
                s.local_ip[3],
                prefix,
                s.gateway_ip[0],
                s.gateway_ip[1],
                s.gateway_ip[2],
                s.gateway_ip[3],
            );
        }
        out
    };

    let bytes = text.as_bytes();
    let copy_len = bytes.len().min(buf_len);
    if !unsafe { try_copy_to_user(buf_ptr, bytes.as_ptr(), copy_len) } {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }
    copy_len as u64
}
