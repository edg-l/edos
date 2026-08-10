//! Exercises positional I/O, process ids and the wall clock.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::io::{
    AT_FDCWD, F_OK, R_OK, Timespec, UTIME_NOW, UTIME_OMIT, W_OK, X_OK, access, pread, pwrite,
    readlink, set_file_times, stat, symlink, truncate, utimensat,
};
use edos_lib::process;
use edos_lib::time;

const CHUNK: usize = 512;
const THREADS: usize = 8;

fn fail(test: u32, msg: &str) -> ! {
    eprintln!("FAIL test {}: {}", test, msg);
    std::process::exit(1);
}

fn pass(test: u32, detail: &str) {
    println!("PASS test {}: {}", test, detail);
}

/// Byte pattern for chunk `i`, distinct per chunk so a misread offset shows up
/// as the wrong chunk rather than as zeros.
fn pattern(i: usize) -> u8 {
    (i as u8).wrapping_mul(37).wrapping_add(11)
}

// -----------------------------------------------------------------------
// Test 1: pwrite/pread round-trip, and neither moves the fd's own offset
// -----------------------------------------------------------------------
fn test1(dir: &str) {
    let path = format!("{}/iotest_t1.dat", dir);
    fs::write(&path, vec![0u8; CHUNK * 4]).unwrap_or_else(|e| fail(1, &format!("create: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(1, &format!("open: {}", e)));
    let fd = file.as_raw_fd() as u64;

    let written = vec![0xABu8; CHUNK];
    let n = pwrite(fd, &written, (CHUNK * 2) as u64);
    if n != CHUNK as isize {
        fail(1, &format!("pwrite returned {}, expected {}", n, CHUNK));
    }

    let mut got = vec![0u8; CHUNK];
    let n = pread(fd, &mut got, (CHUNK * 2) as u64);
    if n != CHUNK as isize {
        fail(1, &format!("pread returned {}, expected {}", n, CHUNK));
    }
    if got != written {
        fail(1, "pread did not return what pwrite wrote");
    }

    // The descriptor's own offset must still be 0: a plain read now has to see
    // the start of the file, which is the zeros, not the pattern at CHUNK*2.
    let mut head = [0u8; 8];
    let mut file = file;
    file.read_exact(&mut head)
        .unwrap_or_else(|e| fail(1, &format!("read after pread: {}", e)));
    if head != [0u8; 8] {
        fail(
            1,
            &format!("positional I/O moved the fd offset; read {:?}", head),
        );
    }

    // And what pwrite wrote must be on disk, not only in this fd's view.
    let whole = fs::read(&path).unwrap_or_else(|e| fail(1, &format!("re-read: {}", e)));
    if whole[CHUNK * 2..CHUNK * 3] != written[..] {
        fail(1, "pwrite did not reach the file");
    }

    let _ = fs::remove_file(&path);
    pass(1, "pwrite/pread round-trip, fd offset untouched");
}

// -----------------------------------------------------------------------
// Test 2: concurrent positional reads through one shared descriptor
//
// This is the case lseek+read cannot express: the threads share one fd, so
// they share one offset, and any seek-then-read pair can be split by another
// thread's seek.
// -----------------------------------------------------------------------
fn test2(dir: &str) {
    let path = format!("{}/iotest_t2.dat", dir);
    let mut content = vec![0u8; CHUNK * THREADS];
    for i in 0..THREADS {
        content[i * CHUNK..(i + 1) * CHUNK].fill(pattern(i));
    }
    fs::write(&path, &content).unwrap_or_else(|e| fail(2, &format!("create: {}", e)));

    let file = Arc::new(File::open(&path).unwrap_or_else(|e| fail(2, &format!("open: {}", e))));
    let fd = file.as_raw_fd() as u64;

    let mut handles = Vec::new();
    for i in 0..THREADS {
        let file = Arc::clone(&file);
        handles.push(thread::spawn(move || {
            // Keep the Arc alive for the thread's lifetime so the fd cannot be
            // closed while another thread is mid-read.
            let _keepalive = &file;
            let mut bad = 0;
            for _ in 0..64 {
                let mut buf = vec![0u8; CHUNK];
                let n = pread(fd, &mut buf, (i * CHUNK) as u64);
                if n != CHUNK as isize || buf.iter().any(|&b| b != pattern(i)) {
                    bad += 1;
                }
            }
            bad
        }));
    }

    let bad: u32 = handles
        .into_iter()
        .map(|h| h.join().unwrap_or_else(|_| fail(2, "thread panicked")))
        .sum();

    let _ = fs::remove_file(&path);

    if bad != 0 {
        fail(
            2,
            &format!(
                "{} of {} concurrent preads read the wrong chunk",
                bad,
                THREADS * 64
            ),
        );
    }
    pass(
        2,
        &format!(
            "{} threads x 64 preads on one shared fd, all correct",
            THREADS
        ),
    );
}

// -----------------------------------------------------------------------
// Test 3: positional I/O is refused on a descriptor with no offset
// -----------------------------------------------------------------------
fn test3() {
    let mut buf = [0u8; 16];
    // stdin is a terminal or a pipe here, never a regular file.
    let n = pread(0, &mut buf, 0);
    if n >= 0 {
        fail(
            3,
            &format!("pread on stdin returned {}, expected failure", n),
        );
    }
    let n = pread(9999, &mut buf, 0);
    if n >= 0 {
        fail(3, &format!("pread on a closed fd returned {}", n));
    }
    pass(3, "pread refused on a non-seekable fd and on a bad fd");
}

// -----------------------------------------------------------------------
// Test 4: process credentials are readable
// -----------------------------------------------------------------------
fn test4() {
    let uid = process::getuid();
    let gid = process::getgid();
    pass(4, &format!("getuid={} getgid={}", uid, gid));
}

// -----------------------------------------------------------------------
// Test 5: the wall clock has a date, moves, and is finer than a second
// -----------------------------------------------------------------------
fn test5() {
    let first = time::clock_gettime_nanos().unwrap_or_else(|| fail(5, "clock_gettime failed"));

    // Any plausible boot is well after 2020-01-01 and well before 2100-01-01.
    const Y2020: u64 = 1_577_836_800_000_000_000;
    const Y2100: u64 = 4_102_444_800_000_000_000;
    if first < Y2020 || first > Y2100 {
        fail(5, &format!("epoch nanos {} is not a plausible date", first));
    }

    let mut moved_sub_second = false;
    for _ in 0..1000 {
        let now = time::clock_gettime_nanos().unwrap_or_else(|| fail(5, "clock_gettime failed"));
        if now < first {
            fail(5, &format!("clock went backwards: {} then {}", first, now));
        }
        if now != first && now - first < 1_000_000_000 {
            moved_sub_second = true;
            break;
        }
    }
    if !moved_sub_second {
        fail(
            5,
            "clock did not advance within a second: no sub-second resolution",
        );
    }

    let t = time::clock_gettime().unwrap_or_else(|| fail(5, "clock_gettime failed"));
    if t.month < 1 || t.month > 12 || t.day < 1 || t.day > 31 || t.hour > 23 {
        fail(5, &format!("implausible broken-down time {:?}", t));
    }
    pass(
        5,
        &format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC ({} ns since epoch)",
            t.year, t.month, t.day, t.hour, t.minute, t.second, first
        ),
    );
}

// -----------------------------------------------------------------------
// Test 6: access() answers for files, directories and missing paths
// -----------------------------------------------------------------------
fn test6(dir: &str) {
    let path = format!("{}/iotest_t6.dat", dir);
    fs::write(&path, b"access").unwrap_or_else(|e| fail(6, &format!("create: {}", e)));

    for (mode, name) in [
        (F_OK, "F_OK"),
        (R_OK, "R_OK"),
        (W_OK, "W_OK"),
        (R_OK | W_OK, "R_OK|W_OK"),
    ] {
        if access(&path, mode) != 0 {
            fail(
                6,
                &format!("access({}, {}) denied an existing file", path, name),
            );
        }
    }

    // A directory is searchable, and the root always exists.
    if access(dir, X_OK) != 0 {
        fail(6, &format!("access({}, X_OK) denied a directory", dir));
    }
    if access("/", F_OK) != 0 {
        fail(6, "access(/, F_OK) says the root does not exist");
    }

    let missing = format!("{}/iotest_t6_missing.dat", dir);
    if access(&missing, F_OK) == 0 {
        fail(6, "access reported a nonexistent path as present");
    }

    // The file has to be gone the moment it is unlinked, not at the next sync.
    fs::remove_file(&path).unwrap_or_else(|e| fail(6, &format!("remove: {}", e)));
    if access(&path, F_OK) == 0 {
        fail(6, "access still sees a file that was unlinked");
    }

    // Bits outside R_OK|W_OK|X_OK are not a mode.
    if access("/", 0x40) == 0 {
        fail(6, "access accepted an undefined mode bit");
    }

    pass(6, "access: existence, modes, directories, unlink, bad mode");
}

// -----------------------------------------------------------------------
// Test 7: nanosleep() honours sub-millisecond requests and rejects non-durations
// -----------------------------------------------------------------------
fn test7() {
    for (sec, nanos, what) in [
        (0i64, 1_000_000_000i64, "nanos == one second"),
        (0, -1, "negative nanos"),
        (-1, 0, "negative seconds"),
    ] {
        if time::nanosleep(sec, nanos) == 0 {
            fail(7, &format!("nanosleep accepted {}", what));
        }
    }

    // 20 sleeps of 500us must add up. A millisecond-granularity sleep would
    // round each one to zero and return immediately.
    let start = Instant::now();
    for _ in 0..20 {
        if time::nanosleep(0, 500_000) != 0 {
            fail(7, "nanosleep(0, 500_000) failed");
        }
    }
    let elapsed = start.elapsed();
    if elapsed < Duration::from_millis(10) {
        fail(
            7,
            &format!(
                "20 x 500us slept only {:?}, so sub-ms requests are dropped",
                elapsed
            ),
        );
    }

    // A single longer sleep must not return early. The monotonic clock is
    // microsecond-resolution, so allow one millisecond of truncation.
    let start = Instant::now();
    if time::nanosleep(0, 120_000_000) != 0 {
        fail(7, "nanosleep(0, 120ms) failed");
    }
    let elapsed = start.elapsed();
    if elapsed < Duration::from_millis(119) {
        fail(
            7,
            &format!("nanosleep(0, 120ms) returned after {:?}", elapsed),
        );
    }

    pass(
        7,
        "nanosleep: sub-ms resolution, full duration, bad requests rejected",
    );
}

// -----------------------------------------------------------------------
// Test 8: path-based truncate() resizes and refuses what it cannot resize
// -----------------------------------------------------------------------
fn test8(dir: &str) {
    let path = format!("{}/iotest_t8.dat", dir);
    fs::write(&path, vec![0xCDu8; CHUNK]).unwrap_or_else(|e| fail(8, &format!("create: {}", e)));

    for size in [100u64, 300, 0] {
        if truncate(&path, size) != 0 {
            fail(8, &format!("truncate to {} failed", size));
        }
        let got = fs::metadata(&path).unwrap_or_else(|e| fail(8, &format!("stat: {}", e)));
        if got.len() != size {
            fail(8, &format!("truncate to {} left {} bytes", size, got.len()));
        }
    }

    // Contents survive a shrink, and the bytes a later grow exposes read as
    // zero. Written after the size checks so the file starts from a known
    // length rather than whatever the loop left.
    fs::write(&path, vec![0xCDu8; CHUNK]).unwrap_or_else(|e| fail(8, &format!("rewrite: {}", e)));
    if truncate(&path, 100) != 0 {
        fail(8, "truncate to 100 failed");
    }
    let kept = fs::read(&path).unwrap_or_else(|e| fail(8, &format!("read after shrink: {}", e)));
    if kept.len() != 100 || kept.iter().any(|&b| b != 0xCD) {
        fail(
            8,
            &format!(
                "shrink lost data: {} bytes, first {:#x}",
                kept.len(),
                kept.first().copied().unwrap_or(0)
            ),
        );
    }
    if truncate(&path, 300) != 0 {
        fail(8, "truncate to 300 failed");
    }
    let grown = fs::read(&path).unwrap_or_else(|e| fail(8, &format!("read after grow: {}", e)));
    if grown.len() != 300 || grown[..100] != kept[..] || grown[100..].iter().any(|&b| b != 0) {
        fail(8, "grow after shrink did not zero-fill");
    }

    if truncate(dir, 0) == 0 {
        fail(8, "truncate resized a directory");
    }
    let missing = format!("{}/iotest_t8_missing.dat", dir);
    if truncate(&missing, 0) == 0 {
        fail(8, "truncate accepted a nonexistent path");
    }

    let _ = fs::remove_file(&path);
    pass(
        8,
        "truncate: shrink keeps data, grow zero-fills, directory and missing refused",
    );
}

// -----------------------------------------------------------------------
// Test 9: utimensat() stamps the times a later stat reports
// -----------------------------------------------------------------------
fn test9(dir: &str) {
    let path = format!("{}/iotest_t9.dat", dir);
    fs::write(&path, b"t9").unwrap_or_else(|e| fail(9, &format!("create: {}", e)));

    // Even seconds: the on-disk encoding keeps 2-second ticks, so an odd
    // second would not survive the round trip.
    const ATIME: i64 = 1_000_000_000;
    const MTIME: i64 = 1_100_000_000;
    if set_file_times(&path, ATIME, MTIME) != 0 {
        fail(9, "set_file_times failed");
    }
    let got = stat(&path).unwrap_or_else(|| fail(9, "stat after set_file_times"));
    if got.accessed != ATIME as u64 || got.modified != MTIME as u64 {
        fail(
            9,
            &format!(
                "times not stamped: accessed {} modified {}",
                got.accessed, got.modified
            ),
        );
    }

    // UTIME_OMIT leaves its timestamp alone; UTIME_NOW moves it forward.
    let times = [
        Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW,
        },
        Timespec {
            tv_sec: 0,
            tv_nsec: UTIME_OMIT,
        },
    ];
    if utimensat(AT_FDCWD, &path, Some(&times), 0) != 0 {
        fail(9, "utimensat with UTIME_NOW/UTIME_OMIT failed");
    }
    let got = stat(&path).unwrap_or_else(|| fail(9, "stat after UTIME_NOW"));
    if got.modified != MTIME as u64 {
        fail(9, "UTIME_OMIT changed the modification time");
    }
    if got.accessed <= ATIME as u64 {
        fail(9, "UTIME_NOW did not move the access time forward");
    }

    // A bad dirfd, an undefined flag and a missing path are all refused.
    if utimensat(3, &path, None, 0) == 0 {
        fail(9, "utimensat accepted a dirfd other than AT_FDCWD");
    }
    if utimensat(AT_FDCWD, &path, None, 0x4000) == 0 {
        fail(9, "utimensat accepted an undefined flag");
    }
    let missing = format!("{}/iotest_t9_missing.dat", dir);
    if utimensat(AT_FDCWD, &missing, None, 0) == 0 {
        fail(9, "utimensat accepted a nonexistent path");
    }

    let _ = fs::remove_file(&path);
    pass(
        9,
        "utimensat: explicit times stamped, UTIME_NOW/UTIME_OMIT honoured, bad args refused",
    );
}

// -----------------------------------------------------------------------
// Test 10: symlink()/readlink() and resolution through a link
// -----------------------------------------------------------------------
fn test10(dir: &str) {
    let target = format!("{}/iotest_t10_target.dat", dir);
    let link = format!("{}/iotest_t10_link.dat", dir);
    let dangling = format!("{}/iotest_t10_dangling.dat", dir);
    let _ = fs::remove_file(&target);
    let _ = fs::remove_file(&link);
    let _ = fs::remove_file(&dangling);

    fs::write(&target, b"symlinked").unwrap_or_else(|e| fail(10, &format!("write target: {}", e)));

    if symlink(&target, &link) != 0 {
        fail(10, "symlink failed");
    }

    // readlink reports the target verbatim, without a terminating NUL.
    let mut buf = [0u8; 256];
    let n = readlink(&link, &mut buf);
    if n < 0 {
        fail(10, "readlink failed");
    }
    if &buf[..n as usize] != target.as_bytes() {
        fail(10, "readlink returned a different target");
    }

    // A short buffer truncates rather than failing.
    let mut small = [0u8; 4];
    if readlink(&link, &mut small) != 4 || &small != &target.as_bytes()[..4] {
        fail(10, "readlink into a short buffer");
    }

    // Reads and stats through the link see the target's contents.
    let via_link =
        fs::read(&link).unwrap_or_else(|e| fail(10, &format!("read through link: {}", e)));
    if via_link != b"symlinked" {
        fail(10, "read through the link returned the wrong contents");
    }
    let st = stat(&link).unwrap_or_else(|| fail(10, "stat through the link"));
    if st.size != 9 {
        fail(10, "stat through the link reported the wrong size");
    }

    // A relative target resolves against the link's own directory.
    let rel_link = format!("{}/iotest_t10_rel.dat", dir);
    let _ = fs::remove_file(&rel_link);
    if symlink("iotest_t10_target.dat", &rel_link) != 0 {
        fail(10, "symlink with a relative target failed");
    }
    if fs::read(&rel_link).ok().as_deref() != Some(b"symlinked".as_slice()) {
        fail(
            10,
            "relative target did not resolve against the link's directory",
        );
    }

    // A dangling link is legal to create and readable, but not to open.
    if symlink("/iotest_t10_nowhere", &dangling) != 0 {
        fail(10, "symlink to a nonexistent target was refused");
    }
    if readlink(&dangling, &mut buf) != "/iotest_t10_nowhere".len() as i64 {
        fail(10, "readlink of a dangling link");
    }
    if fs::read(&dangling).is_ok() {
        fail(10, "reading through a dangling link succeeded");
    }

    // readlink refuses a plain file, and unlinking a link leaves the target.
    if readlink(&target, &mut buf) >= 0 {
        fail(10, "readlink accepted a regular file");
    }
    fs::remove_file(&link).unwrap_or_else(|e| fail(10, &format!("unlink the link: {}", e)));
    if !fs::metadata(&target).is_ok() {
        fail(10, "unlinking the link removed its target");
    }

    let _ = fs::remove_file(&rel_link);
    let _ = fs::remove_file(&dangling);
    let _ = fs::remove_file(&target);
    pass(
        10,
        "symlink/readlink: absolute, relative and dangling links resolve and unlink correctly",
    );
}

// -----------------------------------------------------------------------
// Test 11: sigprocmask() holds a signal pending instead of acting on it
// -----------------------------------------------------------------------
fn test11() {
    use edos_lib::process::{
        SIG_BLOCK, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIGINT, SIGKILL, sigmask, sigprocmask,
        sys_kill, sys_sigaction,
    };

    let original = sigprocmask(SIG_BLOCK, 0);
    if original < 0 {
        fail(11, "sigprocmask query failed");
    }

    // Blocking returns the mask that was in force, and a query sees the new one.
    if sigprocmask(SIG_BLOCK, sigmask(SIGINT)) != original {
        fail(11, "SIG_BLOCK did not return the previous mask");
    }
    if sigprocmask(SIG_BLOCK, 0) != original | sigmask(SIGINT) as i64 {
        fail(11, "SIGINT is not in the mask after SIG_BLOCK");
    }

    // A blocked SIGINT is accepted and held: the default action would have
    // killed this process, so reaching the next line is the assertion.
    if sys_kill(process::getpid(), SIGINT) != 0 {
        fail(11, "kill with a blocked signal failed");
    }
    if sigprocmask(SIG_BLOCK, 0) != original | sigmask(SIGINT) as i64 {
        fail(11, "the mask changed under a blocked kill");
    }

    // Unblocking delivers what was held, so the disposition has to be SIG_IGN
    // first or this process dies here.
    if sys_sigaction(SIGINT, SIG_IGN as u64) < 0 {
        fail(11, "sigaction SIG_IGN failed");
    }
    if sigprocmask(SIG_UNBLOCK, sigmask(SIGINT)) != original | sigmask(SIGINT) as i64 {
        fail(11, "SIG_UNBLOCK did not return the previous mask");
    }
    if sigprocmask(SIG_BLOCK, 0) != original {
        fail(11, "SIGINT is still in the mask after SIG_UNBLOCK");
    }

    // SIGKILL is silently dropped from the mask rather than refused.
    if sigprocmask(SIG_SETMASK, sigmask(SIGKILL) | sigmask(SIGINT)) != original {
        fail(11, "SIG_SETMASK did not return the previous mask");
    }
    if sigprocmask(SIG_BLOCK, 0) != sigmask(SIGINT) as i64 {
        fail(11, "SIGKILL was accepted into the mask");
    }

    // An unknown operation is refused and leaves the mask alone.
    if sigprocmask(99, 0) != -1 {
        fail(11, "an unknown how was accepted");
    }
    if sigprocmask(SIG_SETMASK, original as u32) != sigmask(SIGINT) as i64 {
        fail(11, "the mask changed under a rejected sigprocmask");
    }

    let _ = sys_sigaction(SIGINT, 0);
    pass(
        11,
        "sigprocmask: blocked signals stay pending, SIGKILL unblockable, bad how refused",
    );
}

// -----------------------------------------------------------------------
// Test 12: getdents() enumerates a directory larger than the buffer
// -----------------------------------------------------------------------
fn test12(dir: &str) {
    use edos_lib::io::{getdents, parse_dents};

    let base = format!("{}/iotest_t12", dir);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir(&base).unwrap_or_else(|e| fail(12, &format!("create dir: {}", e)));

    const COUNT: usize = 40;
    for i in 0..COUNT {
        fs::write(format!("{}/f{:03}", base, i), b"x")
            .unwrap_or_else(|e| fail(12, &format!("create entry: {}", e)));
    }

    // A buffer that cannot hold the whole directory: every entry must still be
    // reported exactly once across the calls needed to drain it.
    let mut buf = [0u8; 256];
    let mut names = Vec::new();
    let mut start = 0usize;
    let mut calls = 0;
    loop {
        let n = getdents(&base, &mut buf, start);
        if n < 0 {
            fail(12, "getdents failed");
        }
        if n == 0 {
            break;
        }
        let entries = parse_dents(&buf, n as usize);
        if entries.is_empty() {
            fail(12, "a non-empty result decoded to no entries");
        }
        start += entries.len();
        calls += 1;
        if calls > COUNT {
            fail(12, "getdents never reported the end of the directory");
        }
        for (entry, name) in entries {
            if entry.name_len as usize != name.len() {
                fail(12, "name_len does not match the name written after it");
            }
            names.push(name);
        }
    }

    if calls < 2 {
        fail(
            12,
            "the whole directory fit in one call, so nothing was streamed",
        );
    }
    names.sort();
    names.dedup();
    let expected: Vec<String> = (0..COUNT).map(|i| format!("f{:03}", i)).collect();
    if names != expected {
        fail(12, "the streamed entries do not match the directory");
    }

    // Past the last entry is end of directory, not an error.
    if getdents(&base, &mut buf, COUNT) != 0 {
        fail(
            12,
            "reading past the last entry did not report end of directory",
        );
    }
    // A buffer too small for the next entry is refused, so a caller cannot
    // mistake it for the end and lose the tail.
    let mut tiny = [0u8; 8];
    if getdents(&base, &mut tiny, 0) != -1 {
        fail(12, "a buffer too small for one entry was accepted");
    }

    let _ = fs::remove_dir_all(&base);
    pass(
        12,
        "getdents: a directory larger than the buffer streams from an entry index",
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let dir = args.get(1).map(|s| s.as_str()).unwrap_or("/tmp");

    println!("iotest: running on [{}]", dir);
    test1(dir);
    test2(dir);
    test3();
    test4();
    test5();
    test6(dir);
    test7();
    test8(dir);
    test9(dir);
    test10(dir);
    test11();
    test12(dir);
    println!("iotest: all tests passed [{}]", dir);
    std::process::exit(0);
}
