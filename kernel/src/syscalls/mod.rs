use core::{arch::naked_asm, ptr};

use alloc::{format, string::ToString, vec::Vec};
use x86_64::{
    VirtAddr,
    instructions::interrupts::enable_and_hlt,
    registers::{
        control::{Cr3, Efer, EferFlags},
        model_specific::{LStar, SFMask, Star},
        rflags::RFlags,
    },
};

use crate::{
    fs::{Error as FsError, PollState},
    gdt::selectors,
    graphics::api::ScreenInfo,
    log,
    logs::LOG_BROADCAST,
    println,
    syscalls::{
        fs::{
            sys_list_mounts, sys_list_partitions, sys_mkdir, sys_mount, sys_rmdir, sys_rmdir_all,
            sys_unlink,
        },
        graphics::DrawRequestInput,
        io::{
            sys_chdir, sys_close, sys_getcwd, sys_ioctl, sys_list_dir, sys_open, sys_poll,
            sys_read, sys_write,
        },
        memory::{sys_mmap, sys_munmap},
    },
    thread::{
        Thread, UserThreadInfo,
        scheduler::{ALIVE_THREADS, EXITED_THREADS, sched, switch_to_kernel_page},
    },
};

mod fs;
mod graphics;
mod io;
mod memory;

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
    LStar::write(VirtAddr::new(syscall_entry as usize as u64));

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
const SYS_IOCTL: u64 = 16;
#[allow(unused)]
const SYS_PIPE: u64 = 22;
const SYS_MMAP: u64 = 9;
const SYS_MUNMAP: u64 = 11;
const SYS_EXIT: u64 = 60;
const SYS_ERRNO: u64 = 0x400;
const SYS_GETPID: u64 = 39; // get process ID
const SYS_SPAWN: u64 = 57; // spawn process
const SYS_DUP2: u64 = 33; // duplicate file descriptor
const SYS_DRAW_RECT: u64 = 100;
const SYS_RENDER: u64 = 101;
const SYS_SCREEN_INFO: u64 = 102;
const SYS_DRAW: u64 = 103;
const SYS_KERNEL_LOGS: u64 = 201;
const SYS_WAIT_PID: u64 = 40;
const SYS_MOUNT: u64 = 202;
const SYS_LIST_PARTITIONS: u64 = 203;
const SYS_MKDIR: u64 = 204;
const SYS_RMDIR: u64 = 205;
const SYS_RMDIR_ALL: u64 = 206;
const SYS_UNLINK: u64 = 207;
const SYS_LIST_MOUNTS: u64 = 208;

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
            ctx.rax = sys_ioctl(fd, request, arg) as u64;
        }
        SYS_KERNEL_LOGS => {
            let buffer_ptr = ctx.rdi as *mut u8;
            let count = ctx.rsi as usize;
            ctx.rax = sys_kernel_log(buffer_ptr, count) as u64;
        }
        SYS_CLOSE => {
            let fd = ctx.rdi;
            ctx.rax = sys_close(fd) as u64;
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
            let fd = ctx.rdi;
            let events_ptr = ctx.rsi as *mut PollState;
            let timeout = ctx.rdx;
            ctx.rax = sys_poll(fd, events_ptr, timeout) as u64;
        }
        SYS_MMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;
            let prot = ctx.rdx as u32;
            let flags = ctx.r10 as u32;

            ctx.rax = sys_mmap(addr, length, prot, flags);
        }
        SYS_MUNMAP => {
            let addr = ctx.rdi;
            let length = ctx.rsi;

            ctx.rax = sys_munmap(addr, length) as u64;
        }
        SYS_EXIT => {
            log!("Exit called with code {:?}", ctx.rdi as i32);
            sched().thread_exit(ctx.rdi as i32);

            loop {
                enable_and_hlt();
            }
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
        SYS_DRAW_RECT => {
            ctx.rax = graphics::sys_draw_rect(ctx.rdi, ctx.rsi, ctx.rdx, ctx.r10, ctx.r8 as u32);
        }
        SYS_RENDER => {
            ctx.rax = graphics::sys_render();
        }
        SYS_SCREEN_INFO => {
            ctx.rax = graphics::sys_screen_info(ctx.rdi as *mut ScreenInfo);
        }
        SYS_DRAW => {
            ctx.rax = graphics::sys_draw(ctx.rdi as *const DrawRequestInput);
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
        _ => {
            ctx.rax = !0u64;
        }
    }
}

pub fn sys_errno() -> u64 {
    let sched = sched();
    sched.current_thread_info().lock().errno as u64
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
        }
    }
}

fn sys_getpid() -> u64 {
    let sched = sched();
    sched.current_thread_info().lock().pid
}

fn sys_waitpid(pid: u64, block: bool, status_ptr: *mut i32) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();
    thread.errno = Errno::Clear;

    if block {
        log!("blocking waitpid not supported yet");
        thread.errno = Errno::EINVAL;
        return !0u64;
    }

    drop(thread);

    if let Some(code) = EXITED_THREADS.write().remove(&pid) {
        if !status_ptr.is_null() {
            unsafe { status_ptr.write(code) };
        }
        return pid;
    }

    if ALIVE_THREADS.read().get(&pid).is_some() {
        return 0;
    }

    let mut thread = info.lock();
    thread.errno = Errno::EINVAL;
    !0u64
}

// TODO: figure out why the syscall gets all logs. it doesnt properly subscribe?
pub fn sys_kernel_log(log_buffer: *mut u8, size: usize) -> i64 {
    let info = sched().current_thread_info();

    info.lock().errno = Errno::Clear;
    if log_buffer.is_null() {
        info.lock().errno = Errno::EINVAL;
        return -1;
    }

    let mut buf = Vec::with_capacity(size);

    // not needed?      x86_64::instructions::interrupts::enable();

    let rx = LOG_BROADCAST.lock().subscribe_or_get();

    // Require a 128 byte space.
    while buf.len() + 128 + 1 < size
        && let Some(log) = rx.try_recv()
    {
        let bytes = log.bytes();
        if buf.len() + bytes.len() + 1 < size {
            buf.extend(bytes);
            buf.push(b'\0');
        }
    }

    unsafe { core::ptr::copy_nonoverlapping(buf.as_ptr(), log_buffer, buf.len()) };

    buf.len() as i64
}

fn sys_pipe(pipefd_ptr: *mut [u64; 2]) -> u64 {
    use crate::thread::pipe::{FileDescriptor, Pipe};
    use alloc::sync::Arc;
    use spin::RwLock;

    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();

    thread.errno = Errno::Clear;

    if pipefd_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return !0u64;
    }

    // Create new pipe
    let pipe = Arc::new(RwLock::new(Pipe::new()));

    // Allocate read and write file descriptors
    let read_fd = thread
        .fd_table
        .allocate_fd(FileDescriptor::Pipe(pipe.clone()));
    let write_fd = thread.fd_table.allocate_fd(FileDescriptor::Pipe(pipe));

    // Copy file descriptor numbers to user space
    let pipefd = [read_fd, write_fd];
    unsafe {
        core::ptr::copy_nonoverlapping(pipefd.as_ptr(), pipefd_ptr as *mut u64, 2);
    }

    0 // Success
}

fn sys_dup2(oldfd: u64, newfd: u64) -> u64 {
    let sched = sched();
    let info = sched.current_thread_info();
    let mut thread = info.lock();

    thread.errno = Errno::Clear;

    // Get the file descriptor we want to duplicate
    let old_fd_descriptor = match thread.fd_table.get_fd(oldfd) {
        Some(fd) => fd.clone(),
        None => {
            thread.errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Close the newfd if it's already in use (but don't fail if it doesn't exist)
    let _ = thread.fd_table.close_fd(newfd);

    // Insert the duplicated descriptor at newfd
    thread.fd_table.insert_fd(newfd, old_fd_descriptor);

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
    let mut thread = info.lock();

    thread.errno = Errno::Clear;

    if path_ptr.is_null() {
        thread.errno = Errno::EFAULT;
        return !0u64;
    }

    let path_bytes = match copy_user_c_string(path_ptr, MAX_PATH_LEN) {
        Some(bytes) if !bytes.is_empty() => bytes,
        _ => {
            thread.errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let path_str = match core::str::from_utf8(&path_bytes) {
        Ok(s) => s,
        Err(_) => {
            thread.errno = Errno::EINVAL;
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

    let path = match resolve_path(path_str, &thread.cwd) {
        Ok(path) => path,
        Err(_) => {
            thread.errno = Errno::EINVAL;
            return !0u64;
        }
    };

    let mut argv_storage: Vec<Vec<u8>> = Vec::new();
    argv_storage.push(format!("{path}").as_bytes().to_vec());

    if !argv_ptr.is_null() {
        let mut total_bytes = 0usize;
        let mut terminated = false;

        for index in 0..MAX_ARGC {
            let current_ptr = unsafe { ptr::read_volatile(argv_ptr.add(index)) };
            if current_ptr.is_null() {
                terminated = true;
                break;
            }

            let arg = match copy_user_c_string(current_ptr, MAX_ARG_LEN) {
                Some(bytes) => bytes,
                None => {
                    thread.errno = Errno::EINVAL;
                    return !0u64;
                }
            };

            total_bytes += arg.len() + 1;
            if total_bytes > MAX_ARG_TOTAL {
                thread.errno = Errno::EINVAL;
                return !0u64;
            }

            argv_storage.push(arg);
        }

        if !terminated && argv_storage.len() == MAX_ARGC {
            thread.errno = Errno::EINVAL;
            return !0u64;
        }
    }

    // Save current cwd for child process
    let child_cwd = thread.cwd.clone();

    // Load ELF file from filesystem
    drop(thread); // Release lock before blocking file operations

    x86_64::instructions::interrupts::enable();

    let mut elf_data = Vec::new();
    let info = match fs_api::file_info(&path) {
        Ok(info) => info,
        Err(_) => {
            sched.current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    match fs_api::read_bytes(&path, elf_data.len(), info.size as usize) {
        Ok(data) => {
            elf_data = data;
        }
        Err(_) => {
            sched.current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    }

    x86_64::instructions::interrupts::disable();

    let cr3 = Cr3::read();
    switch_to_kernel_page();

    let argv_slices: Vec<&[u8]> = argv_storage.iter().map(|arg| arg.as_slice()).collect();

    // Create new user thread from ELF data
    let user_thread = match Thread::new_user(&elf_data, Some(path_str.to_string()), &argv_slices) {
        Ok(thread) => {
            log!(
                "UserThread created successfully, entry point: {:p}",
                thread.context.rip() as *const u8
            );
            thread
        }
        Err(e) => {
            log!("UserThread creation failed: {:?}", e);
            sched.current_thread_info().lock().errno = Errno::EINVAL;
            return !0u64;
        }
    };

    // Create thread info and set up file descriptor redirections
    let mut user_thread_info =
        UserThreadInfo::from_thread(user_thread.user.as_ref().unwrap(), 0, 0, child_cwd);

    // Override standard file descriptors if specified (non-default values)
    if stdin_fd != 0
        && let Some(stdin_desc) = sched
            .current_thread_info()
            .lock()
            .fd_table
            .get_fd(stdin_fd)
            .cloned()
    {
        user_thread_info.fd_table.insert_fd(0, stdin_desc);
    }

    if stdout_fd != 1
        && let Some(stdout_desc) = sched
            .current_thread_info()
            .lock()
            .fd_table
            .get_fd(stdout_fd)
            .cloned()
    {
        user_thread_info.fd_table.insert_fd(1, stdout_desc);
    }

    if stderr_fd != 2
        && let Some(stderr_desc) = sched
            .current_thread_info()
            .lock()
            .fd_table
            .get_fd(stderr_fd)
            .cloned()
    {
        user_thread_info.fd_table.insert_fd(2, stderr_desc);
    }

    let child_pid = user_thread.user.as_ref().unwrap().pid;

    // Queue the new thread for execution
    queue_spawn_thread(user_thread, user_thread_info);

    unsafe { Cr3::write(cr3.0, cr3.1) };

    log!("spawn, returning");

    child_pid
}

fn copy_user_c_string(ptr: *const u8, max_len: usize) -> Option<Vec<u8>> {
    if ptr.is_null() {
        return None;
    }

    let mut buf = Vec::new();
    for idx in 0..max_len {
        let byte = unsafe { ptr::read_volatile(ptr.add(idx)) };
        if byte == 0 {
            return Some(buf);
        }
        buf.push(byte);
    }

    None
}
