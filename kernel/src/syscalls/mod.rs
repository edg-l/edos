use core::{
    arch::naked_asm,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering},
    time::Duration,
};

use alloc::vec;
use alloc::{format, string::ToString, sync::Arc, vec::Vec};
use intrusive_list::Link;
use spin::{Mutex, RwLock};
use x86_64::{
    VirtAddr,
    registers::{
        control::{Cr3, Efer, EferFlags},
        model_specific::{LStar, SFMask, Star},
        rflags::RFlags,
    },
    structures::paging::PageTableFlags,
};

use crate::{
    fs::Error as FsError,
    gdt::selectors,
    log,
    memory::STACK_ALIGNMENT,
    println,
    syscalls::{
        fs::{
            FstatEntry, sys_fstat, sys_list_mounts, sys_list_partitions, sys_mkdir, sys_mount,
            sys_rmdir, sys_rmdir_all, sys_stat, sys_unlink,
        },
        io::{
            SelectFd, sys_chdir, sys_close, sys_getcwd, sys_getrandom, sys_list_dir, sys_open,
            sys_poll, sys_read, sys_write,
        },
        memory::{sys_mmap, sys_munmap},
    },
    thread::{
        MemoryRegion, MemoryRegionType, UserThreadInfo,
        context::CpuContext,
        irqlock::IrqSpinlock,
        mutex::BlockingMutex,
        pipe::{FileDescriptor, Pipe},
        scheduler::{sched, switch_to_kernel_page},
        thread::{
            State, Thread, ThreadId, allocate_thread_id, get_thread_info_by_id, insert_thread,
            insert_thread_info, take_thread_exit_code,
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
mod memory;
mod shm;
mod sync;
mod window;

use self::ioctl::sys_ioctl;
use self::sync::{sys_futex_wait, sys_futex_wake};

/// Properly decrement refcounts when a FileDescriptor is removed from a table
/// without going through sys_close (e.g. dup2 replacing an existing fd).
fn close_fd_refcount(desc: FileDescriptor) {
    match desc {
        FileDescriptor::PipeRead(pipe) => {
            pipe.lock().close_reader().flush();
        }
        FileDescriptor::PipeWrite(pipe) => {
            pipe.lock().close_writer().flush();
        }
        FileDescriptor::PtyMaster(pty) => {
            pty.lock().close_master().flush();
        }
        FileDescriptor::PtySlave(pty) => {
            pty.lock().close_slave().flush();
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
const SYS_MUNMAP: u64 = 11;
const SYS_LSEEK: u64 = 12;
const SYS_FTRUNCATE: u64 = 13;
const SYS_RENAME: u64 = 82;
const SYS_ISATTY: u64 = 15;
const SYS_IOCTL: u64 = 16;
#[allow(unused)]
const SYS_PIPE: u64 = 22;
const SYS_EXIT: u64 = 60;
const SYS_ERRNO: u64 = 0x400;
const SYS_GETPID: u64 = 39; // get process ID
const SYS_SPAWN: u64 = 57; // spawn process
const SYS_DUP: u64 = 32; // duplicate file descriptor assigning lowest unused fd
const SYS_DUP2: u64 = 33; // duplicate file descriptor to specific target
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
const SYS_CLOCK_GETTIME: u64 = 226;
const SYS_OPENPTY: u64 = 227;

extern "C" fn syscall_handler(ctx: *mut SyscallContext) {
    let ctx = unsafe { ctx.as_mut().unwrap() };

    // Beware with some sched() calls, they call hlt which might hang if we don't have interrupts enabled.

    // Note: we may need to call switch_to_kernel_page(); and switch back later.

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
        SYS_MMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;
            let prot = ctx.rdx as u32;
            let flags = ctx.r10 as u32;
            let phys_addr = ctx.r8;

            ctx.rax = sys_mmap(addr, length, prot, flags, phys_addr);
        }
        SYS_MUNMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;

            ctx.rax = sys_munmap(addr, length) as u64;
        }
        SYS_EXIT => {
            log!("Exit called with code {:?}", ctx.rdi as i32);
            sched().thread_exit(ctx.rdi as i32);
        }
        SYS_GETPID => {
            ctx.rax = sys_getpid();
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
        SYS_CLOCK_GETTIME => {
            let buf_ptr = ctx.rdi as *mut u8;
            ctx.rax = sys_clock_gettime(buf_ptr);
        }
        SYS_OPENPTY => {
            let pipefd_ptr = ctx.rdi as *mut [u64; 2];
            ctx.rax = io::sys_openpty(pipefd_ptr);
        }
        _ => {
            ctx.rax = !0u64;
        }
    }
}

pub fn sys_errno() -> u64 {
    let sched = sched();
    sched.current_thread_info().lock().errno as u64
}

fn sys_clock_gettime(buf_ptr: *mut u8) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    info.lock().errno = Errno::Clear;

    if buf_ptr.is_null() {
        info.lock().errno = Errno::EFAULT;
        return !0u64;
    }

    let rtc = crate::drivers::rtc::read_rtc();
    let data: [u8; 8] = [
        rtc.hour,
        rtc.minute,
        rtc.second,
        0,
        rtc.day,
        rtc.month,
        (rtc.year & 0xFF) as u8,
        ((rtc.year >> 8) & 0xFF) as u8,
    ];

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
        }
    }
}

fn sys_getpid() -> u64 {
    let sched = sched();
    sched.current_thread_info().lock().pid
}

fn sys_waitpid(pid: u64, block: bool, status_ptr: *mut i32) -> u64 {
    use crate::thread::thread::EXITED_THREADS;

    let sched = sched();
    let info = sched.current_thread_info();
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
    let current_tid = sched.current_thread().unwrap().id;
    EXITED_THREADS.register_waiter(target, current_tid);

    // Park until the target has exited (peek, don't consume)
    sched.thread_park_while(|| !EXITED_THREADS.has_exited(target));

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
    let scheduler = sched();
    let info = scheduler.current_thread_info();
    info.lock().errno = Errno::Clear;

    let duration = Duration::from_millis(milliseconds);

    scheduler.thread_sleep(duration);

    0
}

fn sys_monotonic_time() -> u64 {
    let scheduler = sched();
    let info = scheduler.current_thread_info();
    info.lock().errno = Errno::Clear;

    // HPET-driven uptime is monotonic with microsecond resolution.
    let micros = crate::timer::uptime_us();
    micros.saturating_mul(1_000)
}

fn sys_pipe(pipefd_ptr: *mut [u64; 2]) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();

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
    let sched = sched();
    let info = sched.current_thread_info();

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

fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();

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

fn sys_spawn(
    path_ptr: *const u8,
    argv_ptr: *const *const u8,
    stdin_fd: u64,
    stdout_fd: u64,
    stderr_fd: u64,
) -> u64 {
    use crate::{fs::api as fs_api, fs::path::Path, thread::util::queue_spawn_thread};

    const MAX_PATH_LEN: usize = 1024;
    const MAX_ARGC: usize = 64;
    const MAX_ARG_LEN: usize = 4096;
    const MAX_ARG_TOTAL: usize = 16 * 1024;

    let sched = sched();
    let info = sched.current_thread_info();

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

    // Resolve path relative to current working directory
    let resolve_path = |path_str: &str, cwd: &Path| -> Result<Path, crate::fs::path::ParseError> {
        if path_str.starts_with('/') {
            Path::parse(path_str).map(|p| p.normalize())
        } else {
            let joined = cwd.join(path_str);
            Ok(joined.normalize())
        }
    };

    let path = match resolve_path(path_str, &info.lock().cwd.lock()) {
        Ok(path) => path,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let mut argv_storage: Vec<Vec<u8>> = Vec::new();
    argv_storage.push(format!("{path}").as_bytes().to_vec());

    if !argv_ptr.is_null() {
        let mut total_bytes = 0usize;
        let mut terminated = false;

        for index in 0..MAX_ARGC {
            let current_ptr = match unsafe { try_read_user(argv_ptr.add(index)) } {
                Some(ptr) => ptr,
                None => {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
            };
            if current_ptr.is_null() {
                terminated = true;
                break;
            }

            let arg = match copy_user_c_string(current_ptr, MAX_ARG_LEN) {
                Ok(bytes) => bytes,
                Err(UAccessError::Fault) => {
                    info.lock().errno = Errno::EFAULT;
                    return !0u64;
                }
                Err(UAccessError::TooLong) => {
                    info.lock().errno = Errno::EINVAL;
                    return !0u64;
                }
            };

            total_bytes += arg.len() + 1;
            if total_bytes > MAX_ARG_TOTAL {
                info.lock().errno = Errno::EINVAL;
                return !0u64;
            }

            argv_storage.push(arg);
        }

        if !terminated {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    }

    // Save current cwd for child process
    let child_cwd = info.lock().cwd.lock().clone();

    // Load ELF file from filesystem

    x86_64::instructions::interrupts::enable();

    let mut elf_data = Vec::new();
    let finfo = match fs_api::file_info(&path) {
        Ok(info) => info,
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    match fs_api::read_bytes(&path, elf_data.len(), finfo.size as usize) {
        Ok(data) => {
            elf_data = data;
        }
        Err(_) => {
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    }

    x86_64::instructions::interrupts::disable();

    let cr3 = Cr3::read();
    switch_to_kernel_page();

    let argv_slices: Vec<&[u8]> = argv_storage.iter().map(|arg| arg.as_slice()).collect();

    // Create new user thread from ELF data
    let user_thread = match Thread::new_user(
        &elf_data,
        Some(path_str.to_string()),
        &argv_slices,
        0,
        0,
        child_cwd,
    ) {
        Ok(thread) => {
            log!("UserThread created successfully");
            thread
        }
        Err(e) => {
            log!("UserThread creation failed: {:?}", e);
            info.lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Create thread info and set up file descriptor redirections
    {
        let child_info = get_thread_info_by_id(user_thread.id).unwrap();
        let user_thread_info = child_info.lock();

        // Copy parent's file descriptors to child's standard streams.
        // Always copy so the child inherits the parent's PTY/pipe fds.
        // Use `info` (captured before interrupts were enabled) rather than
        // sched.current_thread_info(), since the thread may have migrated CPUs.
        let parent_fd_table = info.lock().fd_table.clone();
        let parent_fds = parent_fd_table.lock();

        if let Some(stdin_desc) = parent_fds.get_fd(stdin_fd).cloned() {
            stdin_desc.inc_refcount();
            user_thread_info.fd_table.lock().insert_fd(0, stdin_desc);
        }

        if let Some(stdout_desc) = parent_fds.get_fd(stdout_fd).cloned() {
            stdout_desc.inc_refcount();
            user_thread_info.fd_table.lock().insert_fd(1, stdout_desc);
        }

        if let Some(stderr_desc) = parent_fds.get_fd(stderr_fd).cloned() {
            stderr_desc.inc_refcount();
            user_thread_info.fd_table.lock().insert_fd(2, stderr_desc);
        }
    }

    let child_pid = user_thread.id.0;

    // If the child's stdin (fd 0) is a PTY slave, register this child as the
    // foreground process so Ctrl+C signals are delivered to it.
    {
        let child_info = get_thread_info_by_id(user_thread.id).unwrap();
        let child_fd_table = child_info.lock().fd_table.clone();
        if let Some(FileDescriptor::PtySlave(pty)) = child_fd_table.lock().get_fd(0).cloned() {
            pty.lock().foreground_pid = Some(child_pid);
        }
    }

    // Queue the new thread for execution
    queue_spawn_thread(user_thread);

    unsafe { Cr3::write(cr3.0, cr3.1) };

    child_pid
}

fn sys_clone(
    parent_ctx: &mut SyscallContext,
    func_ptr: u64,
    arg: u64,
    _flags: u64,
    child_stack: u64,
) -> u64 {
    let sched = sched();
    let parent_thread = match sched.current_thread() {
        Some(t) => t,
        None => {
            sched.current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let parent_user = match &parent_thread.user {
        Some(u) => u,
        None => {
            sched.current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Allocate user stack if not provided
    let (user_stack_top, stack_region) = if child_stack == 0 {
        // Allocate a new user stack using internal mmap
        let parent_info = sched.current_thread_info();
        let stack_size = 2 * 1024 * 1024; // 2MB stack

        let stack_bottom = {
            let mut info = parent_info.lock();
            memory::find_free_virtual_address(&mut info, stack_size)
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
            parent_info.lock().errno = Errno::ENOMEM;
            return !0u64;
        }

        let stack_region = MemoryRegion {
            start: stack_bottom,
            size: stack_size,
            flags: page_flags,
            region_type: MemoryRegionType::ThreadLocal,
        };

        let stack_top_aligned =
            (stack_bottom.as_u64() + stack_size) & !(STACK_ALIGNMENT as u64 - 1);

        (stack_top_aligned, Some(stack_region))
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
    let parent_process_regions = parent_user_read.memory_regions.clone();
    let process_stack_top = parent_user_read.process_stack_top.clone();
    let address_space_refs = parent_user_read.address_space_refs.clone();
    let mut tls_template = parent_user_read
        .tls
        .as_ref()
        .map(|tls| tls.template.clone());

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
        let mut manager_guard = memory_manager.lock();
        match crate::thread::thread::allocate_tls_region(&template, child_id, &mut manager_guard) {
            Ok(allocation) => {
                tls_fs_base = allocation.fs_base;
                tls_region = Some(allocation.region);
                tls_runtime = Some(allocation.runtime);
            }
            Err(_) => {
                drop(manager_guard);
                kthread_stack_free(kernel_stack_top);
                sched.current_thread_info().lock().errno = Errno::ENOMEM;
                return !0u64;
            }
        }
        drop(manager_guard);
    }

    let mut child_owned_regions: Vec<MemoryRegion> = stack_region.into_iter().collect();
    if let Some(region) = tls_region.take() {
        child_owned_regions.push(region);
    }

    address_space_refs.fetch_add(1, Ordering::AcqRel);

    let child_user = Arc::new(RwLock::new(crate::thread::UserThread {
        pid: child_id.0,
        cr3,
        memory_manager: memory_manager.clone(),
        memory_regions: parent_process_regions,
        owned_regions: child_owned_regions,
        tls: tls_runtime,
        heap_break: parent_heap_break,
        address_space_refs,
        process_stack_top,
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
        sleep_deadline: AtomicU64::new(0),
        cpu_time_ns: AtomicU64::new(0),
        run_start_tick: AtomicU64::new(0),
        tls_base: AtomicU64::new(tls_fs_base),
        cpu: AtomicU32::new(0),
        exit_code: AtomicI32::new(0),
        killed: AtomicBool::new(false),
        user: Some(child_user),
        rq_link: Link::new(),
        rq_boosted: AtomicBool::new(false),
        context_saved: AtomicBool::new(true),
        fpu: core::cell::UnsafeCell::new(crate::drivers::fpu::FpuState::default()),
        fpu_init: AtomicBool::new(false),
    });

    // Clone parent's UserThreadInfo - share fd_table
    let parent_info = sched.current_thread_info();
    let parent_info_guard = parent_info.lock();

    insert_thread(child_thread.clone());
    insert_thread_info(
        child_id,
        Arc::new(IrqSpinlock::new(UserThreadInfo {
            pid: child_id.0,
            errno: Errno::Clear,
            fd_table: parent_info_guard.fd_table.clone(), // Arc clone - shared
            memory_mappings: parent_info_guard.memory_mappings.clone(), // Arc clone - shared
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
