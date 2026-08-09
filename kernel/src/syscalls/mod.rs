use crate::thread::preempt::PreemptSpinlock;
use core::{
    arch::naked_asm,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
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
    debug::lock_order::{
        RANK_NET_STACK, RANK_PIPE, RANK_PORT_TABLE, RANK_PTY, RANK_SOCKET, RANK_TCP_CONN,
    },
    fs::Error as FsError,
    gdt::selectors,
    log,
    memory::{
        STACK_ALIGNMENT,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt, VmaSet},
    },
    net::device::NetDevice,
    println, ranked_lock,
    syscalls::{
        fs::{
            FstatEntry, sys_access, sys_fstat, sys_list_mounts, sys_list_partitions, sys_mkdir,
            sys_mount, sys_rmdir, sys_rmdir_all, sys_stat, sys_unlink,
        },
        io::{
            SelectFd, sys_chdir, sys_close, sys_getcwd, sys_getrandom, sys_list_dir, sys_open,
            sys_poll, sys_read, sys_write,
        },
        memory::{sys_mmap, sys_msync, sys_munmap},
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
            State, Thread, ThreadId, allocate_thread_id, get_thread_info_by_id, insert_thread,
            insert_thread_info, kill_process_with_signal, take_thread_exit_code,
        },
        util::{kthread_stack_alloc, kthread_stack_free},
    },
    util::uaccess::{
        UAccessError, try_copy_string_from_user, try_copy_to_user, try_read_user, try_write_user,
    },
};

mod fs;
mod io;
mod ioctl;
pub mod memory;
mod net;
mod shm;
mod sync;
mod window;

use self::ioctl::sys_ioctl;
use self::sync::{sys_futex_wait, sys_futex_wake};
use crate::thread::scheduler::{
    current_thread, current_thread_info, current_thread_weak, exit_if_killed, thread_exit,
    thread_park_while, thread_sleep,
};

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
            let bound = s.local_addr.map(|addr| {
                let proto = if s.sock_type == crate::net::socket::SOCK_DGRAM {
                    17u8
                } else {
                    6u8
                };
                (proto, addr.port)
            });
            let tcp_conn = s.tcp_conn.clone();
            drop(s);
            if let Some(key) = bound {
                ranked_lock!(
                    RANK_PORT_TABLE,
                    "fd::drop_socket_port",
                    crate::net::socket::port_table()
                )
                .remove(&key);
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
const SYS_OPEN: u64 = 2;
const SYS_CLOSE: u64 = 3;
const SYS_LIST_DIR: u64 = 4;
const SYS_GETCWD: u64 = 5;
const SYS_CHDIR: u64 = 6;
const SYS_POLL: u64 = 7;
const SYS_FSTAT: u64 = 8;
const SYS_MMAP: u64 = 9;
const SYS_STAT: u64 = 10;
const SYS_ACCESS: u64 = 21; // check a path against an access mode
const SYS_MUNMAP: u64 = 11;
const SYS_LSEEK: u64 = 12;
const SYS_FTRUNCATE: u64 = 13;
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
const SYS_MONOTONIC_TIME: u64 = 210;
const SYS_CLONE: u64 = 211;
const SYS_FUTEX_WAIT: u64 = 212;
const SYS_FUTEX_WAKE: u64 = 213;
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
const SYS_CLOCK_GETTIME: u64 = 226;
const SYS_OPENPTY: u64 = 227;
const SYS_SPAWN2: u64 = 228;
const SYS_KILL: u64 = 229;
const SYS_SIGACTION: u64 = 230;
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
const SYS_SYNC: u64 = 162;

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
    crate::util::per_cpu::get_percpu_data().with_current_thread(|t| {
        t.last_syscall.store(ctx.rax as u32, Ordering::Relaxed);
    });

    match ctx.rax {
        SYS_WRITE => {
            let fd = ctx.rdi;
            let buffer_ptr = ctx.rsi as *const u8;
            let count = ctx.rdx as usize;
            ctx.rax = sys_write(fd, buffer_ptr, count);
        }
        SYS_OPEN => {
            let path_ptr = ctx.rdi as *const u8;
            let flags = ctx.rsi;
            ctx.rax = sys_open(path_ptr, flags) as u64;
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
        SYS_GETUID => {
            ctx.rax = current_thread_info().lock().user_id as u64;
        }
        SYS_GETGID => {
            ctx.rax = current_thread_info().lock().group_id as u64;
        }
        SYS_WAIT_PID => {
            let pid = ctx.rdi;
            let block = ctx.rsi;
            let status_ptr = ctx.rdx as *mut i32;
            ctx.rax = sys_waitpid(pid, block == 1, status_ptr);
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
            let prot = ctx.rdx;
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
            ctx.rax = window::sys_window_list(buffer_ptr, max);
        }
        SYS_WINDOW_SEND_EVENT => {
            let window_id = ctx.rdi;
            let event_ptr = ctx.rsi as *const crate::window::WindowEvent;
            ctx.rax = window::sys_window_send_event(window_id, event_ptr);
        }
        SYS_WINDOW_DAMAGE => {
            let window_id = ctx.rdi;
            ctx.rax = window::sys_window_damage(window_id);
        }
        SYS_CLOCK_GETTIME => {
            let buf_ptr = ctx.rdi as *mut u8;
            ctx.rax = sys_clock_gettime(buf_ptr);
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
            let pid = ctx.rdi;
            let signum = ctx.rsi as u32;
            let info = current_thread_info();
            if signum == 0 || signum >= 32 {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
            } else if kill_process_with_signal(pid, signum) {
                ctx.rax = 0;
            } else {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
            }
        }
        SYS_SIGACTION => {
            let signum = ctx.rdi as u32;
            let handler = ctx.rsi as u32; // 0=SIG_DFL, 1=SIG_IGN
            let info = current_thread_info();
            if signum == 0 || signum >= 32 || signum == signal::SIGKILL {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
            } else if let Some(cur_thread) = current_thread() {
                let prev = cur_thread.signal.set_handler(signum, handler);
                ctx.rax = prev as u64;
            } else {
                info.lock().errno = Errno::EINVAL;
                ctx.rax = !0u64;
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
        SYS_SYNC => {
            io::sys_sync();
            ctx.rax = 0;
        }
        _ => {
            ctx.rax = !0u64;
        }
    }

    // A thread marked for termination dies here rather than returning to user
    // code. This is one of the two boundaries where that is safe; the other is
    // a timer tick that interrupted ring 3, which covers a thread that spins
    // without ever making a syscall.
    exit_if_killed();
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

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(clippy::upper_case_acronyms, unused)]
#[repr(u64)]
pub enum Errno {
    /// No error; used to clear the errno field.
    Clear,
    /// Invalid argument passed to a syscall.
    EINVAL,
    /// Memory allocation failed or memory exhausted.
    ENOMEM,
    /// Bad memory address provided by userspace.
    EFAULT,
    /// Invalid or closed file descriptor.
    EBADF,
    /// Operation requires permissions the caller lacks.
    EACCES,
    /// Operation not permitted for the current caller.
    EPERM,
    /// Requested file or directory does not exist.
    ENOENT,
    /// Attempted to create an entry that already exists.
    EEXIST,
    /// Expected a directory but encountered a non-directory entry.
    ENOTDIR,
    /// Operation required a regular file but encountered a directory.
    EISDIR,
    /// Device or filesystem has no space left for the operation.
    ENOSPC,
    /// Write attempted on a read-only filesystem or device.
    EROFS,
    /// Generic I/O failure surfaced from the filesystem or storage layer.
    EIO,
    /// System call interrupted (e.g. by a signal or kill).
    EINTR,
    /// File format is not recognized as an executable.
    ENOEXEC,
    /// Resource temporarily unavailable; operation would block.
    EAGAIN,
    /// Socket is not connected.
    ENOTCONN,
    /// Connection was refused by the remote host.
    ECONNREFUSED,
    /// Address already in use (bind).
    EADDRINUSE,
    /// Broken pipe: write to a closed connection.
    EPIPE,
    /// Address family not supported (e.g. IPv6 on IPv4-only system).
    EAFNOSUPPORT,
    /// Seek on a descriptor that has no file offset (pipe, socket, tty).
    ESPIPE,
    /// Device or resource is in use, e.g. a disk that backs a live mount.
    EBUSY,
    /// Placeholder for unknown or unmapped kernel error codes.
    UNKNOWN,
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
            FsError::ProtocolMismatch => Errno::EIO,
        }
    }
}

fn sys_getpid() -> u64 {
    current_thread_info().lock().pid
}

fn sys_waitpid(pid: u64, block: bool, status_ptr: *mut i32) -> u64 {
    use crate::thread::thread::EXITED_THREADS;

    let info = current_thread_info();
    info.lock().errno = Errno::Clear;

    let target = ThreadId(pid);

    // Fast path: already exited
    if let Some(code) = take_thread_exit_code(target) {
        if !status_ptr.is_null() && !unsafe { try_write_user(status_ptr, code) } {
            info.lock().errno = Errno::EFAULT;
            return !0u64;
        }
        return pid;
    }

    if !block {
        return 0;
    }

    // Register as waiter so record_thread_exit wakes us
    let current_weak = current_thread_weak().unwrap();
    EXITED_THREADS.register_waiter(target, current_weak);

    // Park until the target has exited. thread_park_while may return
    // spuriously (stale wake token, etc.), so loop on the real condition.
    while !EXITED_THREADS.has_exited(target) {
        thread_park_while(|| !EXITED_THREADS.has_exited(target));
    }

    EXITED_THREADS.unregister_waiter(target);

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

    // Allocate read and write file descriptors
    let read_fd = info
        .lock()
        .fd_table
        .lock()
        .allocate_fd(FileDescriptor::PipeRead(pipe.clone()));
    let write_fd = info
        .lock()
        .fd_table
        .lock()
        .allocate_fd(FileDescriptor::PipeWrite(pipe));

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
        info.lock().fd_table.lock().close_fd(read_fd);
        info.lock().fd_table.lock().close_fd(write_fd);
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
    table.allocate_fd(old_fd_descriptor)
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
/// Supports the descriptor-flag and duplication commands. `F_GETFL`/`F_SETFL`
/// return EINVAL rather than a plausible-looking zero: there is no
/// `O_NONBLOCK` in this kernel, so a caller that "successfully" sets it would
/// be told a lie it then relies on.
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
            let new_fd = table.allocate_fd_from(desc, arg);
            table.set_cloexec(new_fd, cmd == F_DUPFD_CLOEXEC);
            new_fd as i64
        }
        F_GETFL | F_SETFL => {
            drop(table);
            info.lock().errno = Errno::EINVAL;
            -1
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

    // Insert the duplicated descriptor at newfd
    old_fd_descriptor.inc_refcount();
    fd_table.lock().insert_fd(newfd, old_fd_descriptor);

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
        let mut tokens = shebang_line.splitn(2, |c: char| c == ' ' || c == '\t');
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
    let parent_stdin = {
        let fd_table = info.lock().fd_table.clone();
        let fds = fd_table.lock();
        (
            fds.get_fd(stdin_fd).cloned(),
            fds.get_fd(stdout_fd).cloned(),
            fds.get_fd(stderr_fd).cloned(),
        )
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

        if let Some(desc) = stdin_desc {
            desc.inc_refcount();
            user_thread_info.fd_table.lock().insert_fd(0, desc);
        }

        if let Some(desc) = stdout_desc {
            desc.inc_refcount();
            user_thread_info.fd_table.lock().insert_fd(1, desc);
        }

        if let Some(desc) = stderr_desc {
            desc.inc_refcount();
            user_thread_info.fd_table.lock().insert_fd(2, desc);
        }
    }

    let child_pid = user_thread.id.0;

    // If the child's stdin (fd 0) is a PTY slave, register this child as the
    // foreground process so Ctrl+C signals are delivered to it.
    {
        let child_info = get_thread_info_by_id(user_thread.id).unwrap();
        let child_fd_table = child_info.lock().fd_table.clone();
        if let Some(FileDescriptor::PtySlave(pty)) = child_fd_table.lock().get_fd(0).cloned() {
            ranked_lock!(RANK_PTY, "sys_spawn::foreground", pty).foreground_pid = Some(child_pid);
        }
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
    use x86_64::registers::model_specific::FsBase;

    use crate::{
        fs::api as fs_api,
        memory::frame_allocator::frame_allocator,
        thread::{
            pipe::close_descriptor,
            thread::{load_process_image, quiesce_address_space},
        },
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
        let doomed = fd_table.lock().take_cloexec();
        doomed
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
    let (parts, old) = install_image(&user_arc, &info, image);

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
    FsBase::write(VirtAddr::new(tls_fs_base));
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

    user.cr3 = (image.pml4_frame, kernel_pml4_flags);
    user.tls = image.tls;
    user.heap_break = image.heap_break;
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
            &parent_user,
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

        let top = (stack_bottom.as_u64() + stack_size) & !(STACK_ALIGNMENT as u64 - 1);
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

    let mut tls_runtime = None;
    let mut tls_region = None;
    let mut tls_fs_base = 0u64;
    if let Some(template) = tls_template.take() {
        let mut manager_guard = ranked_lock!(RANK_USER_MM, "user.mm", memory_manager);
        let tls_slot = next_tls_slot.fetch_add(1, Ordering::Relaxed);
        match crate::thread::thread::allocate_tls_region(&template, tls_slot, &mut manager_guard) {
            Ok(allocation) => {
                tls_fs_base = allocation.fs_base;
                tls_region = Some(allocation.vma);
                tls_runtime = Some(allocation.runtime);
            }
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
        drop(manager_guard);
    }

    // Add the new TLS VMA to the shared VmaSet; the stack was claimed above.
    if let Some(vma) = tls_region.take() {
        ranked_lock!(RANK_VMAS, "sys_clone::tls_vma_insert", parent_vmas).insert_validated(vma);
    }

    address_space_refs.fetch_add(1, Ordering::AcqRel);

    let child_user = Arc::new(RwLock::new(crate::thread::UserThread {
        pid: child_id.0,
        cr3,
        memory_manager: memory_manager.clone(),
        vmas: parent_vmas, // Arc clone - shared address space
        tls: tls_runtime,
        heap_break: parent_heap_break,
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
        priority: AtomicU8::new(parent_thread.priority()),
        parent: AtomicU64::new(parent_thread.id.0),
        sleep_deadline: AtomicU64::new(0),
        cpu_time_ns: AtomicU64::new(0),
        run_start_tick: AtomicU64::new(0),
        created_at_tick: AtomicU64::new(crate::timer::Instant::now().tick()),
        demand_faults: AtomicU32::new(0),
        tls_base: AtomicU64::new(tls_fs_base),
        cpu: AtomicU32::new(0),
        exit_code: AtomicI32::new(0),
        killed: AtomicBool::new(false),
        wake_pending: AtomicBool::new(false),
        last_syscall: AtomicU32::new(crate::thread::thread::NO_SYSCALL),
        signal: SignalState::new(),
        user: Some(child_user),
        rq_link: Link::new(),
        context_saved: AtomicBool::new(true),
        fpu: core::cell::UnsafeCell::new(crate::drivers::fpu::FpuState::default()),
        fpu_init: AtomicBool::new(false),
        owned_ops: crate::thread::irqlock::IrqSpinlock::new(HeaplessVec::new()),
        #[cfg(debug_assertions)]
        lock_ranks: core::cell::UnsafeCell::new(heapless::Vec::new()),
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
    child_id.0
}

fn sys_fork(parent_ctx: &mut SyscallContext) -> i64 {
    use core::sync::atomic::AtomicUsize;
    use x86_64::structures::paging::OffsetPageTable;

    use crate::{
        memory::{
            cow::clone_user_page_tables_cow,
            mapper::{MemoryManager, get_level_4_table},
            shared::SharedMemory,
        },
        thread::fd::FileDescriptorTable,
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
    let parent_process_stack_top = parent_user_read.process_stack_top.load(Ordering::Acquire);
    let parent_tls = parent_user_read.tls.clone();
    let parent_fs_base = parent_thread.tls_base.load(Ordering::Acquire);

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

    // Clone COW page tables using the parent's VmaSet.
    // Must be called with parent's CR3 active.
    // tlb_shootdown_all() inside flushes all CPUs' stale writable entries.
    let child_pml4_frame = {
        let parent_vmas = ranked_lock!(RANK_VMAS, "user.vmas", parent_user_read.vmas);
        unsafe { clone_user_page_tables_cow(parent_cr3.0, &parent_vmas) }
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

    // Deep-clone the fd_table: new table with cloned entries, refcounts bumped.
    let child_fd_table = {
        let guard = parent_info.lock();
        let parent_fds = guard.fd_table.lock();
        let mut new_table = FileDescriptorTable::new_empty();
        for (fd_num, desc) in parent_fds.iter_all() {
            desc.inc_refcount();
            new_table.insert_fd(fd_num, desc.clone());
        }
        Arc::new(BlockingMutex::new(new_table))
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
    child_ctx.rbx = parent_ctx.rbx;
    child_ctx.rbp = parent_ctx.rbp;
    child_ctx.r12 = parent_ctx.r12;
    child_ctx.r13 = parent_ctx.r13;
    child_ctx.r14 = parent_ctx.r14;
    child_ctx.r15 = parent_ctx.r15;

    let child_id = allocate_thread_id();

    let child_user_arc = Arc::new(RwLock::new(crate::thread::UserThread {
        pid: child_id.0,
        cr3: (child_pml4_frame, parent_cr3.1),
        memory_manager: child_mm.clone(),
        vmas: child_vma_set_arc,
        tls: parent_tls,
        heap_break: parent_heap_break,
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
        priority: AtomicU8::new(parent_thread.priority()),
        parent: AtomicU64::new(parent_thread.id.0),
        sleep_deadline: AtomicU64::new(0),
        cpu_time_ns: AtomicU64::new(0),
        run_start_tick: AtomicU64::new(0),
        created_at_tick: AtomicU64::new(crate::timer::Instant::now().tick()),
        demand_faults: AtomicU32::new(0),
        tls_base: AtomicU64::new(parent_fs_base),
        cpu: AtomicU32::new(0),
        exit_code: AtomicI32::new(0),
        killed: AtomicBool::new(false),
        wake_pending: AtomicBool::new(false),
        last_syscall: AtomicU32::new(crate::thread::thread::NO_SYSCALL),
        signal: SignalState::new(),
        user: Some(child_user_arc.clone()),
        rq_link: Link::new(),
        context_saved: AtomicBool::new(true),
        fpu: {
            // Save parent's current FPU/SSE state and copy to child.
            let mut fpu_state = crate::drivers::fpu::FpuState::default();
            unsafe { crate::drivers::fpu::save_fpu_state(&mut fpu_state) };
            core::cell::UnsafeCell::new(fpu_state)
        },
        fpu_init: AtomicBool::new(true),
        owned_ops: crate::thread::irqlock::IrqSpinlock::new(HeaplessVec::new()),
        #[cfg(debug_assertions)]
        lock_ranks: core::cell::UnsafeCell::new(heapless::Vec::new()),
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
pub fn copy_user_path<'a>(buf: &'a mut PathBuf, ptr: *const u8) -> Result<&'a str, Errno> {
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
    let len = match unsafe { try_copy_string_from_user(buf.as_mut_ptr(), ptr, max_len) } {
        Ok(len) => len,
        Err(err) => return Err(err),
    };

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

    match crate::net::stack::syscall_ping(dst_ip, id, seq, timeout) {
        Some(rtt_us) => rtt_us,
        None => !0u64,
    }
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

    if buf_ptr.is_null() || buf_len == 0 {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let text = {
        use alloc::fmt::Write;
        let mut out = alloc::string::String::with_capacity(256);

        // ANSI: \x1b[1m = bold, \x1b[32m = green, \x1b[31m = red,
        //       \x1b[36m = cyan, \x1b[0m = reset

        // lo - loopback
        let _ = write!(out, "1: \x1b[1mlo\x1b[0m: <LOOPBACK,\x1b[32mUP\x1b[0m>\n");
        let _ = write!(out, "    inet \x1b[36m127.0.0.1/8\x1b[0m\n");

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
