/* Exercises the stub layer through newlib rather than through the raw calls.
 *
 * Everything here goes via a C library function, because the point is that
 * newlib's own machinery works on top of these stubs: stdio buffering reaching
 * `write`, `malloc` reaching `sbrk`, `fopen` reaching `open` and `fstat`, and a
 * failure reaching `errno` with a code the caller can name.
 */

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/times.h>
#include <unistd.h>

static int passed;
static int failed;

static int cmp_int(const void *a, const void *b) {
    return *(const int *)a - *(const int *)b;
}

static void check(const char *name, int ok, const char *detail) {
    if (ok) {
        passed++;
        printf("ok   %s: %s\n", name, detail);
    } else {
        failed++;
        printf("FAIL %s: %s\n", name, detail);
    }
}

int main(void) {
    char buf[128];
    char detail[192];

    /* malloc reaches sbrk, and the arena has to be writable for the whole
     * range rather than only its first page. */
    size_t big = 1u << 20;
    char *p = malloc(big);
    if (p == NULL) {
        check("malloc", 0, "malloc(1 MiB) returned NULL");
    } else {
        memset(p, 0xa5, big);
        int intact = (p[0] == (char)0xa5) && (p[big - 1] == (char)0xa5);
        snprintf(detail, sizeof detail, "1 MiB allocated at %p, first and last byte intact", (void *)p);
        check("malloc", intact, detail);
        free(p);
    }

    /* A second, larger allocation forces sbrk to extend rather than serve from
     * what the first one left over. */
    char *q = malloc(4u << 20);
    check("malloc grows", q != NULL, q ? "4 MiB after 1 MiB" : "second malloc failed");
    free(q);

    /* fopen/fwrite/fclose, then read it back: open, write, lseek, close,
     * fstat (stdio sizes its buffer from it) and read. */
    const char *path = "/var/ctest.txt";
    const char *text = "newlib wrote this\n";
    FILE *f = fopen(path, "w");
    if (f == NULL) {
        check("fopen w", 0, strerror(errno));
    } else {
        size_t n = fwrite(text, 1, strlen(text), f);
        fclose(f);
        snprintf(detail, sizeof detail, "wrote %zu of %zu bytes", n, strlen(text));
        check("fwrite", n == strlen(text), detail);
    }

    f = fopen(path, "r");
    if (f == NULL) {
        check("fopen r", 0, strerror(errno));
    } else {
        size_t n = fread(buf, 1, sizeof buf - 1, f);
        buf[n] = '\0';
        fclose(f);
        snprintf(detail, sizeof detail, "read back %zu bytes: %s", n, buf);
        check("fread", n == strlen(text) && strcmp(buf, text) == 0, detail);
    }

    /* stat on the file just written. */
    struct stat st;
    if (stat(path, &st) != 0) {
        check("stat", 0, strerror(errno));
    } else {
        snprintf(detail, sizeof detail, "size %ld, S_ISREG %d", (long)st.st_size, S_ISREG(st.st_mode) ? 1 : 0);
        check("stat", (size_t)st.st_size == strlen(text) && S_ISREG(st.st_mode), detail);
    }

    /* A directory has to come back as one, since that is the field stdio and
     * every ported program branch on. */
    if (stat("/var", &st) != 0) {
        check("stat dir", 0, strerror(errno));
    } else {
        check("stat dir", S_ISDIR(st.st_mode) != 0, "/var reports S_ISDIR");
    }

    /* fseek/ftell exercise lseek. */
    f = fopen(path, "r");
    if (f == NULL) {
        check("lseek", 0, strerror(errno));
    } else {
        fseek(f, 7, SEEK_SET);
        long pos = ftell(f);
        int c = fgetc(f);
        fclose(f);
        snprintf(detail, sizeof detail, "ftell %ld, byte '%c'", pos, c);
        check("lseek", pos == 7 && c == text[7], detail);
    }

    /* errno has to carry a code that names the failure, which is the whole
     * point of the kernel returning POSIX numbering. */
    errno = 0;
    f = fopen("/var/definitely_not_here", "r");
    snprintf(detail, sizeof detail, "errno %d (%s)", errno, strerror(errno));
    check("errno on missing file", f == NULL && errno == ENOENT, detail);
    if (f != NULL) {
        fclose(f);
    }

    /* getpid, and isatty on a pipe-free descriptor. */
    snprintf(detail, sizeof detail, "getpid %d", (int)getpid());
    check("getpid", getpid() > 0, detail);

    /* gettimeofday should be well past the epoch. */
    struct timeval tv;
    if (gettimeofday(&tv, NULL) != 0) {
        check("gettimeofday", 0, strerror(errno));
    } else {
        snprintf(detail, sizeof detail, "tv_sec %ld", (long)tv.tv_sec);
        check("gettimeofday", tv.tv_sec > 1600000000L, detail);
    }

    /* times() has nothing to answer from and must say so rather than guess. */
    errno = 0;
    struct tms tms_buf;
    clock_t t = times(&tms_buf);
    snprintf(detail, sizeof detail, "returned %ld, errno %d", (long)t, errno);
    check("times reports ENOSYS", t == (clock_t)-1 && errno == ENOSYS, detail);

    /* unlink, then confirm it is gone. */
    if (unlink(path) != 0) {
        check("unlink", 0, strerror(errno));
    } else {
        check("unlink", stat(path, &st) != 0, "file gone after unlink");
    }

    /* Floating point through newlib's own formatter, which is a good part of
     * why a real libc is worth having. */
    snprintf(buf, sizeof buf, "%.3f %e %5.2f", 3.14159, 1234.5, 2.0 / 3.0);
    check("printf floats", strcmp(buf, "3.142 1.234500e+03  0.67") == 0, buf);

    /* qsort and strtod, two things a hand-written shim gets subtly wrong. */
    int nums[] = {5, 3, 9, 1, 7};
    qsort(nums, 5, sizeof(int), cmp_int);
    snprintf(detail, sizeof detail, "%d %d %d %d %d", nums[0], nums[1], nums[2], nums[3], nums[4]);
    check("qsort", nums[0] == 1 && nums[4] == 9, detail);

    double d = strtod("  -2.5e3xyz", NULL);
    snprintf(detail, sizeof detail, "strtod gave %.1f", d);
    check("strtod", d == -2500.0, detail);

    printf("ctest: %d passed, %d failed\n", passed, failed);
    return failed == 0 ? 0 : 1;
}
