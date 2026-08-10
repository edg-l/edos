//! Raw syscall helpers and syscall number constants.

use core::arch::asm;

pub const SYS_READ: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_OPEN: u64 = 2;
pub const SYS_CLOSE: u64 = 3;
pub const SYS_POLL: u64 = 7;
pub const SYS_LSEEK: u64 = 12;
pub const SYS_FTRUNCATE: u64 = 13;
pub const SYS_TRUNCATE: u64 = 76;
pub const SYS_ISATTY: u64 = 15;
pub const SYS_ACCESS: u64 = 21;
pub const SYS_STAT: u64 = 10;
pub const SYS_UTIMENSAT: u64 = 280;
pub const SYS_OPENAT: u64 = 257;
pub const SYS_MKDIRAT: u64 = 258;
pub const SYS_FSTATAT: u64 = 262;
pub const SYS_UNLINKAT: u64 = 263;
pub const SYS_RENAMEAT: u64 = 264;
pub const SYS_SYMLINKAT: u64 = 266;
pub const SYS_READLINKAT: u64 = 267;
pub const SYS_FACCESSAT: u64 = 269;
pub const SYS_SYMLINK: u64 = 88;
pub const SYS_READLINK: u64 = 89;
pub const SYS_GETDENTS: u64 = 78;
pub const SYS_MMAP: u64 = 9;
pub const SYS_MUNMAP: u64 = 11;
pub const SYS_IOCTL: u64 = 16;
pub const SYS_PIPE: u64 = 22;
pub const SYS_DUP: u64 = 32;
pub const SYS_DUP2: u64 = 33;
pub const SYS_PREAD: u64 = 17;
pub const SYS_PWRITE: u64 = 18;
pub const SYS_READV: u64 = 19;
pub const SYS_WRITEV: u64 = 20;
pub const SYS_EXECVE: u64 = 59;
pub const SYS_FCNTL: u64 = 72;
pub const SYS_GETPID: u64 = 39;
pub const SYS_GETUID: u64 = 102;
pub const SYS_GETGID: u64 = 104;
pub const SYS_WAIT_PID: u64 = 40;
pub const SYS_SPAWN: u64 = 57;
pub const SYS_RENAME: u64 = 82;
pub const SYS_NANOSLEEP: u64 = 35;
pub const SYS_CLOCK_GETTIME: u64 = 226;
pub const SYS_OPENPTY: u64 = 227;
pub const SYS_SPAWN2: u64 = 228;
pub const SYS_SHM_CREATE: u64 = 215;
pub const SYS_SHM_MAP: u64 = 216;
pub const SYS_SHM_UNMAP: u64 = 217;
pub const SYS_SHM_DESTROY: u64 = 218;
pub const SYS_SHM_SIZE: u64 = 231;
pub const SYS_KILL: u64 = 229;
pub const SYS_SIGACTION: u64 = 230;
pub const SYS_SIGPROCMASK: u64 = 233;
pub const SYS_SOCKET: u64 = 240;
pub const SYS_BIND: u64 = 241;
pub const SYS_CONNECT: u64 = 242;
pub const SYS_LISTEN: u64 = 243;
pub const SYS_ACCEPT: u64 = 244;
pub const SYS_SENDTO: u64 = 245;
pub const SYS_RECVFROM: u64 = 246;
pub const SYS_PING: u64 = 249;
pub const SYS_NETINFO: u64 = 250;
pub const SYS_FORK: u64 = 255;
pub const SYS_SYNC: u64 = 162;
pub const SYS_MOUNT: u64 = 202;
pub const SYS_LIST_PARTITIONS: u64 = 203;
pub const SYS_LIST_MOUNTS: u64 = 208;
pub const SYS_STATFS: u64 = 254;

pub const AF_INET: u32 = 2;
pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;

/// Raw syscall with 0 arguments.
#[inline(always)]
pub unsafe fn syscall0(num: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 1 argument.
#[inline(always)]
pub unsafe fn syscall1(num: u64, arg1: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 2 arguments.
#[inline(always)]
pub unsafe fn syscall2(num: u64, arg1: u64, arg2: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 3 arguments.
#[inline(always)]
pub unsafe fn syscall3(num: u64, arg1: u64, arg2: u64, arg3: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 4 arguments.
#[inline(always)]
pub unsafe fn syscall4(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 5 arguments.
#[inline(always)]
pub unsafe fn syscall5(num: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall with 6 arguments.
#[inline(always)]
pub unsafe fn syscall6(
    num: u64,
    arg1: u64,
    arg2: u64,
    arg3: u64,
    arg4: u64,
    arg5: u64,
    arg6: u64,
) -> u64 {
    let ret: u64;
    unsafe {
        asm!(
            "syscall",
            in("rax") num,
            in("rdi") arg1,
            in("rsi") arg2,
            in("rdx") arg3,
            in("r10") arg4,
            in("r8") arg5,
            in("r9") arg6,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}
