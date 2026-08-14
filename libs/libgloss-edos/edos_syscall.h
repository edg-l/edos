/* Raw EDOS system call entry, for the newlib stub layer.
 *
 * The convention is the one every x86-64 Unix uses: the number in rax, the
 * arguments in rdi, rsi, rdx, r10, r8, r9, and the result in rax. `syscall`
 * clobbers rcx and r11, which hold the return address and flags.
 *
 * A failure comes back as a negated errno in the [-4095, -1] window, so a code
 * is read straight out of the return register with no translation table. That
 * is the whole reason the kernel uses POSIX's numbering.
 */

#ifndef EDOS_SYSCALL_H
#define EDOS_SYSCALL_H

#include <errno.h>

#define SYS_READ 0
#define SYS_WRITE 1
#define SYS_CLOSE 3
#define SYS_FSTAT 8
#define SYS_MMAP 9
#define SYS_STAT 10
#define SYS_LSEEK 12
#define SYS_ISATTY 15
#define SYS_NANOSLEEP 35
#define SYS_GETPID 39
#define SYS_WAIT_PID 40
#define SYS_EXECVE 59
#define SYS_EXIT 60
#define SYS_UNLINK 207
#define SYS_CLOCK_GETTIME 226
#define SYS_KILL 229
#define SYS_FORK 255
#define SYS_OPENAT 257

/* The largest errno the kernel can return, which bounds the window that
 * separates an error from a large valid result. */
#define EDOS_MAX_ERRNO 4095

/* Directory descriptor meaning "resolve against the working directory". */
#define EDOS_AT_FDCWD (-100)

/* The kernel's open flags. O_RDONLY, O_WRONLY and O_RDWR are 0, 1 and 2 in both
 * newlib and here, but every flag above them has a different bit: newlib's
 * O_CREAT is 0x200 where this is 0x40, and its O_TRUNC is 0x400 where this is
 * O_APPEND. Passing newlib's value straight through opens a file for append
 * when the caller asked to create and truncate it. */
#define EDOS_O_CREAT 0x40
#define EDOS_O_TRUNC 0x200
#define EDOS_O_APPEND 0x400
#define EDOS_O_NONBLOCK 0x800

static inline long edos_syscall6(long nr, long a1, long a2, long a3, long a4,
                                 long a5, long a6) {
    long ret;
    register long r10 __asm__("r10") = a4;
    register long r8 __asm__("r8") = a5;
    register long r9 __asm__("r9") = a6;
    __asm__ volatile("syscall"
                     : "=a"(ret)
                     : "a"(nr), "D"(a1), "S"(a2), "d"(a3), "r"(r10), "r"(r8),
                       "r"(r9)
                     : "rcx", "r11", "memory");
    return ret;
}

#define edos_syscall0(nr) edos_syscall6((nr), 0, 0, 0, 0, 0, 0)
#define edos_syscall1(nr, a) edos_syscall6((nr), (long)(a), 0, 0, 0, 0, 0)
#define edos_syscall2(nr, a, b) edos_syscall6((nr), (long)(a), (long)(b), 0, 0, 0, 0)
#define edos_syscall3(nr, a, b, c) \
    edos_syscall6((nr), (long)(a), (long)(b), (long)(c), 0, 0, 0)
#define edos_syscall4(nr, a, b, c, d) \
    edos_syscall6((nr), (long)(a), (long)(b), (long)(c), (long)(d), 0, 0)

/* The kernel's errno numbering is POSIX's, matching Linux. newlib has its own,
 * and the two agree only across the classic UNIX range: 36 of the kernel's 54
 * codes are identical and 18 are not, starting at `ENAMETOOLONG`. So a
 * translation is needed after all — smaller than it would be against a private
 * numbering, but not empty, and a port that assumes "POSIX numbering means no
 * table" reports the wrong failure for every socket error.
 *
 * Written against newlib's own macros rather than its numbers, so it cannot
 * drift from the headers it is compiled with. The kernel's values are literals
 * because they are the ABI and are not visible from here.
 */
static inline int edos_errno_to_newlib(long code) {
    switch (code) {
        case 1: return EPERM;
        case 2: return ENOENT;
        case 3: return ESRCH;
        case 4: return EINTR;
        case 5: return EIO;
        case 6: return ENXIO;
        case 7: return E2BIG;
        case 8: return ENOEXEC;
        case 9: return EBADF;
        case 10: return ECHILD;
        case 11: return EAGAIN;
        case 12: return ENOMEM;
        case 13: return EACCES;
        case 14: return EFAULT;
        case 16: return EBUSY;
        case 17: return EEXIST;
        case 18: return EXDEV;
        case 19: return ENODEV;
        case 20: return ENOTDIR;
        case 21: return EISDIR;
        case 22: return EINVAL;
        case 23: return ENFILE;
        case 24: return EMFILE;
        case 25: return ENOTTY;
        case 27: return EFBIG;
        case 28: return ENOSPC;
        case 29: return ESPIPE;
        case 30: return EROFS;
        case 31: return EMLINK;
        case 32: return EPIPE;
        case 33: return EDOM;
        case 34: return ERANGE;
        case 36: return ENAMETOOLONG;
        case 38: return ENOSYS;
        case 39: return ENOTEMPTY;
        case 40: return ELOOP;
        case 75: return EOVERFLOW;
        case 88: return ENOTSOCK;
        case 90: return EMSGSIZE;
        case 95: return EOPNOTSUPP;
        case 97: return EAFNOSUPPORT;
        case 98: return EADDRINUSE;
        case 99: return EADDRNOTAVAIL;
        case 101: return ENETUNREACH;
        case 103: return ECONNABORTED;
        case 104: return ECONNRESET;
        case 105: return ENOBUFS;
        case 106: return EISCONN;
        case 107: return ENOTCONN;
        case 110: return ETIMEDOUT;
        case 111: return ECONNREFUSED;
        case 113: return EHOSTUNREACH;
        case 114: return EALREADY;
        case 115: return EINPROGRESS;
        default: return EIO;
    }
}

/* Turns a raw return into newlib's convention: -1 with `errno` set.
 *
 * Testing the window rather than a single sentinel is the whole contract. A
 * check for -1 alone lets every other code through as a valid result, which
 * then flows on as a byte count or an address.
 */
static inline long edos_ret(long ret) {
    if (ret < 0 && ret >= -EDOS_MAX_ERRNO) {
        errno = edos_errno_to_newlib(-ret);
        return -1;
    }
    return ret;
}

#endif /* EDOS_SYSCALL_H */
