/* The OS layer newlib is written against.
 *
 * newlib's only dependency on an operating system is this set of functions
 * (its documentation calls it libgloss). Built with
 * `--disable-newlib-supplied-syscalls`, it calls them by their unprefixed
 * names and expects each to return -1 with `errno` set on failure.
 *
 * Everything here is a thin translation. Where EDOS has no equivalent the
 * function reports `ENOSYS` rather than choosing a nearby code that means
 * something else, which is what a caller needs in order to tell "this system
 * cannot do that" from "that particular call failed".
 */

#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/times.h>
#include <sys/types.h>
#include <unistd.h>

#include "edos_syscall.h"

/* FstatEntry as `syscalls/fs.rs` lays it out. Field for field, or `fstat`
 * reads garbage with nothing to show at compile time. */
struct edos_stat {
    unsigned long size;
    unsigned long created;
    unsigned long accessed;
    unsigned long modified;
    unsigned short attrs;
    unsigned char kind;
};

#define EDOS_KIND_FILE 0
#define EDOS_KIND_DIR 1
#define EDOS_KIND_SYMLINK 2
#define EDOS_KIND_SPECIAL 3
#define EDOS_KIND_FIFO 4

#define EDOS_ATTR_READONLY 1

static void fill_stat(const struct edos_stat *src, struct stat *dst) {
    mode_t mode;

    switch (src->kind) {
        case EDOS_KIND_DIR: mode = S_IFDIR; break;
        case EDOS_KIND_SYMLINK: mode = S_IFLNK; break;
        case EDOS_KIND_SPECIAL: mode = S_IFCHR; break;
        case EDOS_KIND_FIFO: mode = S_IFIFO; break;
        default: mode = S_IFREG; break;
    }

    /* EDOS has no permission bits, only a read-only flag (see the set_attrs
     * gap in the kernel), so the mode is synthesised from it. */
    mode |= (src->attrs & EDOS_ATTR_READONLY) ? 0444 : 0666;
    if (src->kind == EDOS_KIND_DIR) {
        mode |= 0111;
    }

    dst->st_mode = mode;
    dst->st_size = (off_t)src->size;
    dst->st_atime = (time_t)src->accessed;
    dst->st_mtime = (time_t)src->modified;
    dst->st_ctime = (time_t)src->created;
    dst->st_nlink = 1;
    dst->st_blksize = 4096;
    dst->st_blocks = (blkcnt_t)((src->size + 511) / 512);
}

static size_t cstr_len(const char *s) {
    size_t n = 0;
    while (s[n] != '\0') {
        n++;
    }
    return n;
}

/* newlib picks this return type itself; matching the macro rather than
 * spelling `ssize_t` is what keeps the definition and its header in step. */
_READ_WRITE_RETURN_TYPE read(int fd, void *buf, size_t count) {
    return (_READ_WRITE_RETURN_TYPE)edos_ret(edos_syscall3(SYS_READ, fd, buf, count));
}

_READ_WRITE_RETURN_TYPE write(int fd, const void *buf, size_t count) {
    return (_READ_WRITE_RETURN_TYPE)edos_ret(edos_syscall3(SYS_WRITE, fd, buf, count));
}

/* newlib's open flags into the kernel's. Only the access mode in the low two
 * bits is shared; see the note in edos_syscall.h. */
static long open_flags_to_edos(int flags) {
    long out = flags & 3;

    if (flags & O_CREAT) {
        out |= EDOS_O_CREAT;
    }
    if (flags & O_TRUNC) {
        out |= EDOS_O_TRUNC;
    }
    if (flags & O_APPEND) {
        out |= EDOS_O_APPEND;
    }
    if (flags & O_NONBLOCK) {
        out |= EDOS_O_NONBLOCK;
    }
    return out;
}

int open(const char *path, int flags, ...) {
    /* SYS_OPENAT takes pointer plus length, unlike SYS_OPEN's NUL-terminated
     * form. Both entry points exist in the kernel; this is the one that does
     * not need the caller to guarantee a terminator it did not write. */
    return (int)edos_ret(edos_syscall4(SYS_OPENAT, EDOS_AT_FDCWD, path,
                                       cstr_len(path),
                                       open_flags_to_edos(flags)));
}

int close(int fd) {
    return (int)edos_ret(edos_syscall1(SYS_CLOSE, fd));
}

off_t lseek(int fd, off_t offset, int whence) {
    return (off_t)edos_ret(edos_syscall3(SYS_LSEEK, fd, offset, whence));
}

int fstat(int fd, struct stat *st) {
    struct edos_stat entry;
    long ret = edos_ret(edos_syscall2(SYS_FSTAT, fd, &entry));

    if (ret < 0) {
        return -1;
    }
    fill_stat(&entry, st);
    return 0;
}

int stat(const char *path, struct stat *st) {
    struct edos_stat entry;
    long ret =
        edos_ret(edos_syscall3(SYS_STAT, path, cstr_len(path), &entry));

    if (ret < 0) {
        return -1;
    }
    fill_stat(&entry, st);
    return 0;
}

int isatty(int fd) {
    /* The odd one out: it answers 0 or 1 rather than reporting an error, so a
     * failure has to become "not a terminal". */
    long ret = edos_syscall1(SYS_ISATTY, fd);
    return ret == 1 ? 1 : 0;
}

int unlink(const char *path) {
    return (int)edos_ret(edos_syscall1(SYS_UNLINK, path));
}

int link(const char *existing, const char *newpath) {
    /* EDOS has symbolic links but no hard links, and inventing one out of a
     * symlink would make `st_nlink` and unlink semantics lie. */
    (void)existing;
    (void)newpath;
    errno = ENOSYS;
    return -1;
}

pid_t getpid(void) {
    return (pid_t)edos_syscall0(SYS_GETPID);
}

int kill(pid_t pid, int sig) {
    return (int)edos_ret(edos_syscall2(SYS_KILL, pid, sig));
}

pid_t fork(void) {
    return (pid_t)edos_ret(edos_syscall0(SYS_FORK));
}

int execve(const char *path, char *const argv[], char *const envp[]) {
    return (int)edos_ret(edos_syscall3(SYS_EXECVE, path, argv, envp));
}

pid_t wait(int *status) {
    /* SYS_WAIT_PID with pid 0 waits for any child; the second argument is the
     * blocking flag the kernel's own wrapper passes. */
    return (pid_t)edos_ret(edos_syscall3(SYS_WAIT_PID, 0, 1, status));
}

clock_t times(struct tms *buf) {
    /* Per-process CPU accounting is not exposed by the kernel. Reporting
     * ENOSYS is the honest answer and is why the errno list carries it. */
    (void)buf;
    errno = ENOSYS;
    return (clock_t)-1;
}

int gettimeofday(struct timeval *tv, void *tz) {
    unsigned long nanos = 0;

    (void)tz;
    if (edos_ret(edos_syscall1(SYS_CLOCK_GETTIME, &nanos)) < 0) {
        return -1;
    }
    tv->tv_sec = (time_t)(nanos / 1000000000UL);
    tv->tv_usec = (suseconds_t)((nanos % 1000000000UL) / 1000UL);
    return 0;
}

void _exit(int status) {
    edos_syscall1(SYS_EXIT, status);
    __builtin_unreachable();
}

/* The heap.
 *
 * The kernel has no `brk`, so the break lives over one anonymous mapping made
 * on first use. Anonymous mappings are demand-faulted, so reserving the whole
 * arena costs a VMA and no memory until a page is touched; that is what lets a
 * single reservation stand in for a growable break, which `mmap` alone cannot
 * provide because a second mapping need not land adjacent to the first.
 */
#define EDOS_HEAP_SIZE (64UL * 1024 * 1024)
#define EDOS_PROT_READ 0x1
#define EDOS_PROT_WRITE 0x2
#define EDOS_MAP_PRIVATE 0x02
#define EDOS_MAP_ANONYMOUS 0x20

static char *heap_base;
static char *heap_end;
static char *heap_break;

void *sbrk(ptrdiff_t increment) {
    char *previous;

    if (heap_base == 0) {
        long addr = edos_syscall6(SYS_MMAP, 0, EDOS_HEAP_SIZE,
                                  EDOS_PROT_READ | EDOS_PROT_WRITE,
                                  EDOS_MAP_PRIVATE | EDOS_MAP_ANONYMOUS, -1, 0);
        if (addr < 0 && addr >= -EDOS_MAX_ERRNO) {
            errno = (int)-addr;
            return (void *)-1;
        }
        heap_base = (char *)addr;
        heap_break = heap_base;
        heap_end = heap_base + EDOS_HEAP_SIZE;
    }

    previous = heap_break;
    if (increment > 0 && (size_t)(heap_end - heap_break) < (size_t)increment) {
        errno = ENOMEM;
        return (void *)-1;
    }
    heap_break += increment;
    return previous;
}
