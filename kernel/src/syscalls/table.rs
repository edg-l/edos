//! One description of the syscall surface: number, name, the function behind
//! it, and what each argument is.
//!
//! Three consumers share it so none can hold a private copy that rots.
//! [`super::dispatch`] expands it into the match that calls each
//! implementation, the tracer reads it to decide which arguments are strings
//! worth copying out of user memory, and `/proc/syscalls` publishes it so
//! `strace` formats calls from the kernel's own table instead of a duplicate
//! of it.

use edos_trace_abi::ArgKind;

/// Every syscall this kernel answers.
///
/// An entry is `number, "name", function, (kind: type, ...)`. The kind is what
/// the tracer formats the argument as; the type is what the function takes.
/// Arguments map onto `rdi, rsi, rdx, r10, r8, r9` in that order, which is the
/// SYSCALL ABI's argument sequence. A list beginning with `ctx` hands the
/// function the whole saved context first, which is what a call that rewrites
/// its caller's registers needs.
///
/// The list takes the name of a macro and hands itself to it, because its two
/// expansions land in different modules: `syscall_rows!` below builds the
/// [`SYSCALLS`] array, and `super::syscall_arms!` builds the dispatch match
/// where the implementations are in scope. Neither writes a `sys_*` signature,
/// so every implementation stays where a reader can see it.
macro_rules! syscall_table {
    ($emit:ident) => {
        $emit! {
        SYS_READ, "read", io::sys_read, (Fd: u64, Out: *mut u8, Len: usize);
        SYS_WRITE, "write", io::sys_write, (Fd: u64, StrLen: *const u8, Len: usize);
        SYS_CLOSE, "close", io::sys_close, (Fd: u64);
        SYS_LIST_DIR, "list_dir", io::sys_list_dir, (Str: *const u8, Buf: *mut u8, Len: usize);
        SYS_GETCWD, "getcwd", io::sys_getcwd, (Buf: *mut u8, Len: usize);
        SYS_CHDIR, "chdir", io::sys_chdir, (Str: *const u8);
        SYS_POLL, "poll", io::sys_poll, (Ptr: *mut SelectFd, Len: usize, Int: u64);
        SYS_FSTAT, "fstat", fs::sys_fstat, (Fd: u64, Ptr: *mut Stat);
        SYS_MMAP, "mmap", memory::sys_mmap, (Ptr: u64, Len: u64, Hex: u32, Hex: u32, Hex: u64, Len: u64);
        SYS_STAT, "stat", fs::sys_stat, (StrLen: *const u8, Len: usize, Ptr: *mut Stat);
        SYS_MUNMAP, "munmap", memory::sys_munmap, (Ptr: u64, Len: u64);
        SYS_MPROTECT, "mprotect", memory::sys_mprotect, (Ptr: u64, Len: u64, Hex: u32);
        SYS_LSEEK, "lseek", io::sys_lseek, (Fd: u64, Int: i64, Int: u32);
        SYS_FTRUNCATE, "ftruncate", io::sys_ftruncate, (Fd: u64, Len: u64);
        SYS_FSYNC, "fsync", io::sys_fsync, (Fd: u64);
        SYS_ISATTY, "isatty", io::sys_isatty, (Fd: u64);
        SYS_IOCTL, "ioctl", ioctl::sys_ioctl, (Fd: u64, Hex: u64, Ptr: u64, Len: usize, Hex: u64);
        SYS_PREAD, "pread", io::sys_pread, (Fd: u64, Out: *mut u8, Len: usize, Len: u64);
        SYS_PWRITE, "pwrite", io::sys_pwrite, (Fd: u64, StrLen: *const u8, Len: usize, Len: u64);
        SYS_READV, "readv", io::sys_readv, (Fd: u64, Ptr: *const io::IoVec, Len: usize);
        SYS_WRITEV, "writev", io::sys_writev, (Fd: u64, Ptr: *const io::IoVec, Len: usize);
        SYS_ACCESS, "access", fs::sys_access, (StrLen: *const u8, Len: usize, Hex: u32);
        SYS_PIPE, "pipe", sys_pipe, (Ptr: *mut [u64; 2]);
        SYS_DUP, "dup", sys_dup, (Fd: u64);
        SYS_DUP2, "dup2", sys_dup2, (Fd: u64, Fd: u64);
        SYS_MSYNC, "msync", memory::sys_msync, (Ptr: u64, Len: u64, Hex: u32);
        SYS_NANOSLEEP, "nanosleep", sys_nanosleep, (Ptr: *const Timespec, Ptr: *mut Timespec);
        SYS_GETPID, "getpid", sys_getpid, ();
        SYS_WAIT_PID, "waitpid", sys_waitpid, (Int: u64, Hex: u64, Ptr: *mut i32);
        SYS_SETPGID, "setpgid", sys_setpgid, (Int: u64, Int: u64);
        SYS_GETPGID, "getpgid", sys_getpgid, (Int: u64);
        SYS_TCSETPGRP, "tcsetpgrp", io::sys_tcsetpgrp, (Fd: u64, Int: u64);
        SYS_TCGETPGRP, "tcgetpgrp", io::sys_tcgetpgrp, (Fd: u64);
        SYS_SPAWN, "spawn", sys_spawn, (Str: *const u8, Ptr: *const *const u8, Fd: u64, Fd: u64, Fd: u64);
        SYS_EXECVE, "execve", sys_execve, (ctx, Str: *const u8, Ptr: *const *const u8, Ptr: *const *const u8);
        SYS_EXIT, "exit", sys_exit, (Int: i32);
        SYS_FCNTL, "fcntl", sys_fcntl, (Fd: u64, Int: u64, Hex: u64);
        SYS_TRUNCATE, "truncate", fs::sys_truncate, (StrLen: *const u8, Len: usize, Len: u64);
        SYS_GETDENTS, "getdents", io::sys_getdents, (StrLen: *const u8, Len: usize, Buf: *mut u8, Len: usize, Len: usize);
        SYS_RENAME, "rename", io::sys_rename, (Str: *const u8, Str: *const u8);
        SYS_SYMLINK, "symlink", fs::sys_symlink, (StrLen: *const u8, Len: usize, StrLen: *const u8, Len: usize);
        SYS_READLINK, "readlink", fs::sys_readlink, (StrLen: *const u8, Len: usize, Out: *mut u8, Len: usize);
        SYS_GETTID, "gettid", sys_gettid, ();
        SYS_GETUID, "getuid", sys_getuid, ();
        SYS_GETGID, "getgid", sys_getgid, ();
        SYS_SYNC, "sync", io::sys_sync, ();
        SYS_REBOOT, "reboot", sys_reboot, (Int: u64);
        SYS_MOUNT, "mount", fs::sys_mount, (Int: u64, Int: u64, Str: *const u8, Str: *const u8);
        SYS_LIST_PARTITIONS, "list_partitions", fs::sys_list_partitions, (Buf: *mut u8, Len: u64);
        SYS_MKDIR, "mkdir", fs::sys_mkdir, (Str: *const u8);
        SYS_RMDIR, "rmdir", fs::sys_rmdir, (Str: *const u8);
        SYS_RMDIR_ALL, "rmdir_all", fs::sys_rmdir_all, (Str: *const u8);
        SYS_UNLINK, "unlink", fs::sys_unlink, (Str: *const u8);
        SYS_LIST_MOUNTS, "list_mounts", fs::sys_list_mounts, (Buf: *mut u8, Len: usize);
        SYS_SLEEP_MS, "sleep_ms", sys_sleep_ms, (Len: u64);
        SYS_MONOTONIC_TIME, "monotonic_time", sys_monotonic_time, ();
        SYS_CLONE, "clone", sys_clone, (ctx, Ptr: u64, Hex: u64, Hex: u64, Ptr: u64);
        SYS_FUTEX_WAIT, "futex_wait", sync::sys_futex_wait, (Ptr: *const u32, Hex: u32, Len: u64);
        SYS_FUTEX_WAIT_PI, "futex_wait_pi", sync::sys_futex_wait_pi, (Ptr: *const u32, Hex: u32, Len: u64, Int: u64);
        SYS_FUTEX_WAKE, "futex_wake", sync::sys_futex_wake, (Ptr: *const u32, Len: u32);
        SYS_GETRANDOM, "getrandom", io::sys_getrandom, (Buf: *mut u8, Len: usize, Hex: u64);
        SYS_SHM_CREATE, "shm_create", shm::sys_shm_create, (Len: u64);
        SYS_SHM_MAP, "shm_map", shm::sys_shm_map, (Int: u64, Ptr: u64, Hex: u32);
        SYS_SHM_UNMAP, "shm_unmap", shm::sys_shm_unmap, (Ptr: u64);
        SYS_SHM_DESTROY, "shm_destroy", shm::sys_shm_destroy, (Int: u64);
        SYS_WINDOW_CREATE, "window_create", window::sys_window_create, (Int: i64, Int: i64, Len: u64, Len: u64);
        SYS_WINDOW_DESTROY, "window_destroy", window::sys_window_destroy, (Int: crate::window::registry::WindowId);
        SYS_WINDOW_SET, "window_set", window::sys_window_set, (Int: crate::window::registry::WindowId, Int: u64, Hex: u64);
        SYS_WINDOW_GET, "window_get", window::sys_window_get, (Int: crate::window::registry::WindowId, Int: u64);
        SYS_WINDOW_POLL, "window_poll", window::sys_window_poll, (Int: crate::window::registry::WindowId, Ptr: *mut crate::window::WindowEvent, Len: u64);
        SYS_WINDOW_LIST, "window_list", window::sys_window_list, (Buf: *mut u8, Len: u64, Hex: u64);
        SYS_WINDOW_SEND_EVENT, "window_send_event", window::sys_window_send_event, (Int: crate::window::registry::WindowId, Ptr: *const crate::window::WindowEvent);
        SYS_CLOCK_GETTIME, "clock_gettime", sys_clock_gettime, (Buf: *mut u8);
        SYS_CLOCK_SETTIME, "clock_settime", sys_clock_settime, (Ptr: *const u8);
        SYS_SCHED_YIELD, "sched_yield", sys_sched_yield, ();
        SYS_SCHED_SETATTR, "sched_setattr", sys_sched_setattr, (Int: u64, Ptr: *const SchedAttr);
        SYS_SCHED_GETATTR, "sched_getattr", sys_sched_getattr, (Int: u64, Ptr: *mut SchedAttr);
        SYS_OPENPTY, "openpty", io::sys_openpty, (Ptr: *mut [u64; 2]);
        SYS_SPAWN2, "spawn2", sys_spawn2, (Ptr: *const SpawnArgs);
        SYS_KILL, "kill", sys_kill, (Int: i64, Int: u32);
        SYS_SIGACTION, "sigaction", sys_sigaction, (Int: u32, Hex: u64, Hex: u64);
        SYS_SIGRETURN, "sigreturn", sigframe::sys_sigreturn, (ctx);
        SYS_SHM_SIZE, "shm_size", shm::sys_shm_size, (Int: u64);
        SYS_WINDOW_DAMAGE, "window_damage", window::sys_window_damage, (Int: crate::window::registry::WindowId, Int: u32, Int: u32, Int: u32, Int: u32);
        SYS_WINDOW_WAIT, "window_wait", window::sys_window_wait, (Int: crate::window::registry::WindowId, Int: u64, Int: u64);
        SYS_WINDOW_PRESENT, "window_present", window::sys_window_present, ();
        SYS_SIGPROCMASK, "sigprocmask", sys_sigprocmask, (Int: u32, Hex: u32);
        SYS_WINDOW_GRANT_SHELL, "window_grant_shell", window::sys_window_grant_shell, (Int: u64);
        SYS_WINDOW_GRAB_KEY, "window_grab_key", window::sys_window_grab_key, (Int: u64, Hex: u64, Int: u64);
        SYS_CLIPBOARD_GET, "clipboard_get", window::sys_clipboard_get, (Int: u64, Buf: *mut u8, Len: usize);
        SYS_CLIPBOARD_SET, "clipboard_set", window::sys_clipboard_set, (Int: u64, StrLen: *const u8, Len: usize);
        SYS_TRACE_CTL, "trace_ctl", trace::sys_trace_ctl, (Int: u64, Len: u64);
        SYS_TRACE_READ, "trace_read", trace::sys_trace_read, (Buf: *mut edos_trace_abi::TraceRecord, Len: u64, Len: u64);
        SYS_PROFILE_CTL, "profile_ctl", profile::sys_profile_ctl, (Int: u64, Len: u64);
        SYS_PROFILE_READ, "profile_read", profile::sys_profile_read, (Buf: *mut edos_profile_abi::Sample, Len: u64, Len: u64);
        SYS_SOCKET, "socket", net::sys_socket, (Int: u64, Int: u64, Int: u64);
        SYS_BIND, "bind", net::sys_bind, (Fd: u64, Ptr: *const net::SockAddrIn, Len: u64);
        SYS_CONNECT, "connect", net::sys_connect, (Fd: u64, Ptr: *const net::SockAddrIn, Len: u64);
        SYS_LISTEN, "listen", net::sys_listen, (Fd: u64, Len: u32);
        SYS_ACCEPT, "accept", net::sys_accept, (Fd: u64, Ptr: *mut net::SockAddrIn, Ptr: *mut u32);
        SYS_SENDTO, "sendto", net::sys_sendto, (Fd: u64, StrLen: *const u8, Len: u64, Hex: u64, Ptr: *const net::SockAddrIn, Len: u64);
        SYS_RECVFROM, "recvfrom", net::sys_recvfrom, (Fd: u64, Out: *mut u8, Len: u64, Hex: u64, Ptr: *mut net::SockAddrIn, Ptr: *mut u32);
        SYS_SHUTDOWN, "shutdown", net::sys_shutdown, (Fd: u64, Int: u64);
        SYS_SETSOCKOPT, "setsockopt", net::sys_setsockopt, (Fd: u64, Int: i32, Int: i32, Ptr: *const u8, Len: u32);
        SYS_PING, "ping", sys_ping, (Ptr: *const [u8; 4], Int: u16, Int: u16, Len: u64);
        SYS_NETINFO, "netinfo", sys_netinfo, (Buf: *mut u8, Len: usize);
        SYS_GETSOCKOPT, "getsockopt", net::sys_getsockopt, (Fd: u64, Int: i32, Int: i32, Ptr: *mut u8, Ptr: *mut u32);
        SYS_GETPEERNAME, "getpeername", net::sys_getpeername, (Fd: u64, Ptr: *mut net::SockAddrIn, Ptr: *mut u32);
        SYS_GETSOCKNAME, "getsockname", net::sys_getsockname, (Fd: u64, Ptr: *mut net::SockAddrIn, Ptr: *mut u32);
        SYS_STATFS, "statfs", fs::sys_statfs, (Str: *const u8, Buf: *mut u8, Len: usize);
        SYS_FORK, "fork", sys_fork, (ctx);
        SYS_GETDNS, "getdns", net::sys_getdns, (Buf: *mut [u8; 4]);
        SYS_SETDNS, "setdns", net::sys_setdns, (Ptr: *const [u8; 4]);
        SYS_OPENAT, "openat", io::sys_openat, (Fd: i64, StrLen: *const u8, Len: usize, Hex: u64);
        SYS_MKDIRAT, "mkdirat", fs::sys_mkdirat, (Fd: i64, StrLen: *const u8, Len: usize);
        SYS_MKFIFOAT, "mkfifoat", fs::sys_mkfifoat, (Fd: i64, StrLen: *const u8, Len: usize);
        SYS_FSTATAT, "fstatat", fs::sys_fstatat, (Fd: i64, StrLen: *const u8, Len: usize, Ptr: *mut Stat, Hex: u64);
        SYS_UNLINKAT, "unlinkat", fs::sys_unlinkat, (Fd: i64, StrLen: *const u8, Len: usize, Hex: u64);
        SYS_RENAMEAT, "renameat", fs::sys_renameat, (Fd: i64, StrLen: *const u8, Len: usize, Fd: i64, StrLen: *const u8, Len: usize);
        SYS_SYMLINKAT, "symlinkat", fs::sys_symlinkat, (StrLen: *const u8, Len: usize, Fd: i64, StrLen: *const u8, Len: usize);
        SYS_READLINKAT, "readlinkat", fs::sys_readlinkat, (Fd: i64, StrLen: *const u8, Len: usize, Out: *mut u8, Len: usize);
        SYS_FACCESSAT, "faccessat", fs::sys_faccessat, (Fd: i64, StrLen: *const u8, Len: usize, Hex: u32, Hex: u64);
        SYS_UTIMENSAT, "utimensat", fs::sys_utimensat, (Fd: i64, StrLen: *const u8, Len: usize, Ptr: *const fs::UserTimespec, Hex: u64);
        SYS_ERRNO, "errno", sys_errno, ();
        }
    };
}
pub(super) use syscall_table;

pub struct SyscallInfo {
    pub nr: u64,
    pub name: &'static str,
    pub args: &'static [ArgKind],
}

/// The kinds an entry's argument list names, with the types dropped: the row
/// says what an argument means, not what Rust calls it.
macro_rules! arg_kinds {
    (ctx) => { &[] };
    (ctx, $($kind:ident : $ty:ty),*) => { &[$(ArgKind::$kind),*] };
    ($($kind:ident : $ty:ty),*) => { &[$(ArgKind::$kind),*] };
}

macro_rules! syscall_rows {
    ($($nr:ident, $name:literal, $f:path, ($($args:tt)*);)*) => {
        /// Every number [`super::dispatch`] answers, in the table's order.
        pub static SYSCALLS: &[SyscallInfo] = &[
            $(SyscallInfo {
                nr: super::$nr,
                name: $name,
                args: arg_kinds!($($args)*),
            }),*
        ];
    };
}

syscall_table!(syscall_rows);

pub fn lookup(nr: u64) -> Option<&'static SyscallInfo> {
    SYSCALLS.iter().find(|info| info.nr == nr)
}
