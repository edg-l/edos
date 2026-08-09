//! Exercises positional I/O, process ids and the wall clock.

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::io::{F_OK, R_OK, W_OK, X_OK, access, pread, pwrite, truncate};
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
            &format!("shrink lost data: {} bytes, first {:#x}", kept.len(), kept
                .first()
                .copied()
                .unwrap_or(0)),
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
    println!("iotest: all tests passed [{}]", dir);
    std::process::exit(0);
}
