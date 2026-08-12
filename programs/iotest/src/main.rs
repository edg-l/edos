//! Exercises positional I/O, process ids and the wall clock.
//!
//! `can_vector` is unstable upstream and this is a nightly-only target anyway:
//! whether `File` *reports* vectored support is the thing test 19 checks, since
//! a platform that quietly says no gets the one-buffer-at-a-time fallback and
//! passes every other check.
#![feature(can_vector)]

use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use edos_lib::io::{
    AT_FDCWD, F_OK, R_OK, Timespec, UTIME_NOW, UTIME_OMIT, W_OK, X_OK, access, close, futimens,
    open, pread, pwrite, readlink, set_file_times, stat, symlink, truncate, utimensat,
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

    // A bad dirfd, an undefined flag and a missing path are all refused. An
    // absolute path ignores dirfd, so the bad one has to be paired with a
    // relative name to be reached at all.
    if utimensat(4242, "iotest_t9.dat", None, 0) == 0 {
        fail(9, "utimensat accepted a closed dirfd");
    }
    if utimensat(AT_FDCWD, &path, None, 0x4000) == 0 {
        fail(9, "utimensat accepted an undefined flag");
    }
    let missing = format!("{}/iotest_t9_missing.dat", dir);
    if utimensat(AT_FDCWD, &missing, None, 0) == 0 {
        fail(9, "utimensat accepted a nonexistent path");
    }

    // No path stamps the file the descriptor already names, which is the only
    // form reachable from a runtime that hands out descriptors and not names.
    const FUTIME: i64 = 1_200_000_000;
    let fd = open(&path, 0);
    if fd < 0 {
        fail(9, "open for futimens failed");
    }
    let times = [
        Timespec {
            tv_sec: FUTIME,
            tv_nsec: 0,
        },
        Timespec {
            tv_sec: FUTIME,
            tv_nsec: 0,
        },
    ];
    if futimens(fd as u64, Some(&times)) != 0 {
        fail(9, "futimens failed");
    }
    close(fd as u64);
    let got = stat(&path).unwrap_or_else(|| fail(9, "stat after futimens"));
    if got.modified != FUTIME as u64 {
        fail(9, "futimens did not stamp the modification time");
    }

    let _ = fs::remove_file(&path);
    pass(
        9,
        "utimensat: explicit times stamped, UTIME_NOW/UTIME_OMIT honoured, futimens stamps an \
         open file, bad args refused",
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

    // An absolute target names a path in the mount namespace, not one under
    // the root of whichever filesystem the link happens to live on, so it
    // resolves the same whether or not it crosses a mount. `/var` is EFS and
    // `/tmp` is memfs, so one of these two crosses whichever `dir` is.
    for cross in ["/var/iotest_t10_xmount.dat", "/tmp/iotest_t10_xmount.dat"] {
        let cross_link = format!("{}/iotest_t10_xmount_link.dat", dir);
        let _ = fs::remove_file(&cross_link);
        let _ = fs::remove_file(cross);
        fs::write(cross, b"across")
            .unwrap_or_else(|e| fail(10, &format!("write {}: {}", cross, e)));
        if symlink(cross, &cross_link) != 0 {
            fail(10, "symlink to another mount failed");
        }
        if fs::read(&cross_link).ok().as_deref() != Some(b"across".as_slice()) {
            fail(10, "an absolute target resolved in the wrong namespace");
        }
        // Unlinking the link leaves the file it named alone, on either mount.
        fs::remove_file(&cross_link)
            .unwrap_or_else(|e| fail(10, &format!("unlink the cross-mount link: {}", e)));
        if fs::read(cross).ok().as_deref() != Some(b"across".as_slice()) {
            fail(10, "unlinking a cross-mount link removed its target");
        }
        let _ = fs::remove_file(cross);
    }

    // `..` in a relative target walks out of the mount the link lives on, and
    // has to keep walking in the namespace above it.
    let up_target = if dir == "/tmp" { "/var" } else { "/tmp" };
    let up_file = format!("{}/iotest_t10_up.dat", up_target);
    let up_link = format!("{}/iotest_t10_up_link.dat", dir);
    let _ = fs::remove_file(&up_file);
    let _ = fs::remove_file(&up_link);
    fs::write(&up_file, b"upwards")
        .unwrap_or_else(|e| fail(10, &format!("write up target: {}", e)));
    if symlink(&format!("..{}/iotest_t10_up.dat", up_target), &up_link) != 0 {
        fail(10, "symlink with a target above the mount failed");
    }
    if fs::read(&up_link).ok().as_deref() != Some(b"upwards".as_slice()) {
        fail(
            10,
            "a target reached with .. resolved in the wrong namespace",
        );
    }
    let _ = fs::remove_file(&up_link);
    let _ = fs::remove_file(&up_file);

    // Operations that act on a name rather than on what it names: renaming a
    // link moves the link, and `rmdir` refuses one even when it points at an
    // empty directory. Both used to reach through to the target, which is how
    // a rename could turn a link into a second name for its target and an
    // rmdir could delete a directory nobody named.
    let dir_target = format!("{}/iotest_t10_dir", dir);
    let dir_link = format!("{}/iotest_t10_dirlink", dir);
    let moved_link = format!("{}/iotest_t10_moved.dat", dir);
    let _ = fs::remove_file(&dir_link);
    let _ = fs::remove_file(&moved_link);
    let _ = fs::remove_dir(&dir_target);
    fs::create_dir(&dir_target).unwrap_or_else(|e| fail(10, &format!("create dir: {}", e)));
    if symlink(&dir_target, &dir_link) != 0 {
        fail(10, "symlink to a directory failed");
    }
    if fs::remove_dir(&dir_link).is_ok() {
        fail(10, "rmdir removed a symbolic link to a directory");
    }
    if !fs::metadata(&dir_target).is_ok() {
        fail(10, "rmdir of a link removed the directory it named");
    }
    // Renaming the link moves the link: the target keeps its own name, and the
    // moved name still reads as a link to it.
    fs::rename(&dir_link, &moved_link)
        .unwrap_or_else(|e| fail(10, &format!("rename a link: {}", e)));
    if readlink(&moved_link, &mut buf) != dir_target.len() as i64 {
        fail(10, "the renamed link no longer names its target");
    }
    if !fs::metadata(&dir_target).is_ok() {
        fail(10, "renaming a link moved the directory it named");
    }
    let _ = fs::remove_file(&moved_link);
    let _ = fs::remove_dir(&dir_target);

    // A path whose *directory* is a symbolic link has to work end to end:
    // creating a file through it, then writing and reading back through the
    // descriptor that open returned. The descriptor caches where the file
    // lives, so a create that resolves the link while the cache does not
    // leaves an fd that fails every later read and write.
    let via_dir = format!("{}/iotest_t10_via", dir);
    let via_link = format!("{}/iotest_t10_vialink", dir);
    let _ = fs::remove_file(&via_link);
    let _ = fs::remove_file(&format!("{}/f.dat", via_dir));
    let _ = fs::remove_dir(&via_dir);
    fs::create_dir(&via_dir).unwrap_or_else(|e| fail(10, &format!("create via dir: {}", e)));
    if symlink(&via_dir, &via_link) != 0 {
        fail(10, "symlink to the containing directory failed");
    }
    let through = format!("{}/f.dat", via_link);
    fs::write(&through, b"through a linked directory")
        .unwrap_or_else(|e| fail(10, &format!("create through a linked directory: {}", e)));
    if fs::read(&through).ok().as_deref() != Some(b"through a linked directory".as_slice()) {
        fail(
            10,
            "a file created through a linked directory did not read back",
        );
    }
    // And it really landed in the directory the link names.
    if fs::read(&format!("{}/f.dat", via_dir)).ok().as_deref()
        != Some(b"through a linked directory".as_slice())
    {
        fail(10, "the file did not land in the directory the link names");
    }
    let _ = fs::remove_file(&through);
    let _ = fs::remove_file(&via_link);
    let _ = fs::remove_dir(&via_dir);

    // Running a program through a symbolic link. The loader resolves the
    // binary's inode by a path of its own, separate from every resolution
    // above, so it is its own case: `spawn` returning a failure here while
    // `read` of the same path succeeds is exactly what a missed resolution
    // looks like.
    let exec_link = format!("{}/iotest_t10_true", dir);
    let _ = fs::remove_file(&exec_link);
    if symlink("/bin/true", &exec_link) != 0 {
        fail(10, "symlink to a binary failed");
    }
    let pid = process::spawn(&exec_link, &[], 0, 1, 2);
    if pid == u64::MAX || pid == 0 {
        fail(10, "spawning a program through a symbolic link failed");
    }
    if process::waitpid(pid) != 0 {
        fail(
            10,
            "a program spawned through a symbolic link did not exit 0",
        );
    }
    let _ = fs::remove_file(&exec_link);

    // Two links naming each other must run out of hops rather than out of
    // patience. Absolute targets, so each hop goes back through the VFS: this
    // is the case where resolution restarts from the root and could otherwise
    // never stop.
    let loop_a = format!("{}/iotest_t10_loop_a.dat", dir);
    let loop_b = format!("{}/iotest_t10_loop_b.dat", dir);
    let _ = fs::remove_file(&loop_a);
    let _ = fs::remove_file(&loop_b);
    if symlink(&loop_b, &loop_a) != 0 || symlink(&loop_a, &loop_b) != 0 {
        fail(10, "could not build a symlink loop");
    }
    if fs::read(&loop_a).is_ok() {
        fail(10, "reading through a symlink loop succeeded");
    }
    if fs::metadata(&loop_a).is_ok() {
        fail(10, "stat through a symlink loop succeeded");
    }
    // The link itself is still readable: only following it is refused.
    if readlink(&loop_a, &mut buf) != loop_b.len() as i64 {
        fail(10, "readlink of a looping link");
    }
    fs::remove_file(&loop_a).unwrap_or_else(|e| fail(10, &format!("unlink a looping link: {}", e)));
    let _ = fs::remove_file(&loop_b);

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
        "symlink/readlink: absolute, relative, cross-mount, looping and dangling links resolve and unlink correctly",
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

// -----------------------------------------------------------------------
// Test 13: the *at family resolves against a directory descriptor
// -----------------------------------------------------------------------
fn test13(dir: &str) {
    use edos_lib::io::{
        AT_REMOVEDIR, AT_SYMLINK_NOFOLLOW, fstatat, mkdirat, open, openat, symlink, unlinkat,
    };
    use edos_lib::process::close;

    let base = format!("{}/iotest_t13", dir);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir(&base).unwrap_or_else(|e| fail(13, &format!("create dir: {}", e)));

    let dirfd = open(&base, 0);
    if dirfd < 0 {
        fail(13, "cannot open a directory as a descriptor");
    }
    let dirfd = dirfd as u64;

    // A relative name resolves against the descriptor, not the working
    // directory, so the file lands inside `base`.
    let fd = openat(dirfd as i64, "made", 0x40 | 1);
    if fd < 0 {
        fail(13, "openat with O_CREAT failed");
    }
    if pwrite(fd as u64, b"at", 0) != 2 {
        fail(13, "write through an openat descriptor failed");
    }
    close(fd as u64);

    match fstatat(dirfd as i64, "made", 0) {
        Some(st) if st.size == 2 => {}
        Some(st) => fail(13, &format!("fstatat reported {} bytes, want 2", st.size)),
        None => fail(13, "fstatat cannot see the file openat created"),
    }
    if fs::read(format!("{}/made", base)).unwrap_or_default() != b"at" {
        fail(13, "openat resolved against the wrong directory");
    }

    // An absolute path ignores dirfd entirely.
    let abs = format!("{}/made", base);
    let fd = openat(dirfd as i64, &abs, 0);
    if fd < 0 {
        fail(13, "openat refused an absolute path");
    }
    let mut buf = [0u8; 2];
    if pread(fd as u64, &mut buf, 0) != 2 || &buf != b"at" {
        fail(13, "read back the wrong contents through openat");
    }
    close(fd as u64);

    if mkdirat(dirfd as i64, "sub") != 0 {
        fail(13, "mkdirat failed");
    }
    match fstatat(dirfd as i64, "sub", 0) {
        Some(st) if st.kind == 1 => {}
        _ => fail(13, "mkdirat did not create a directory"),
    }

    // AT_REMOVEDIR is the rmdir/unlink switch, and each refuses the other's
    // kind rather than removing it.
    if unlinkat(dirfd as i64, "sub", 0) == 0 {
        fail(13, "unlinkat without AT_REMOVEDIR removed a directory");
    }
    if unlinkat(dirfd as i64, "sub", AT_REMOVEDIR) != 0 {
        fail(13, "unlinkat with AT_REMOVEDIR failed");
    }
    if unlinkat(dirfd as i64, "made", 0) != 0 {
        fail(13, "unlinkat failed to remove a file");
    }
    if fstatat(dirfd as i64, "made", 0).is_some() {
        fail(13, "the file survived unlinkat");
    }

    // A descriptor that is not a directory, and one that is not open at all,
    // are both refused; AT_FDCWD keeps the working-directory meaning.
    let filefd = open(&format!("{}/plain", base), 0x40 | 1);
    if filefd < 0 {
        fail(13, "cannot create a plain file");
    }
    if openat(filefd, "x", 0) != -1 {
        fail(13, "openat accepted a descriptor that is not a directory");
    }
    close(filefd as u64);
    if openat(4242, "x", 0) != -1 {
        fail(13, "openat accepted a closed descriptor");
    }
    if fstatat(AT_FDCWD, &base, 0).is_none() {
        fail(13, "fstatat with AT_FDCWD cannot see an absolute path");
    }
    // AT_SYMLINK_NOFOLLOW describes the link itself: on a path that is not a
    // link it agrees with a plain stat, and on one that is it reports kind 2
    // and the length of the target rather than the target's own size.
    if fstatat(AT_FDCWD, &base, AT_SYMLINK_NOFOLLOW).is_none() {
        fail(13, "fstatat refused AT_SYMLINK_NOFOLLOW");
    }
    let target = format!("{}/plain", base);
    let link = format!("{}/alias", base);
    if symlink(&target, &link) != 0 {
        fail(13, "cannot create a symbolic link");
    }
    match (
        fstatat(AT_FDCWD, &link, 0),
        fstatat(AT_FDCWD, &link, AT_SYMLINK_NOFOLLOW),
    ) {
        (Some(followed), Some(itself)) => {
            if followed.kind == 2 {
                fail(
                    13,
                    "fstatat without the flag described the link, not its target",
                );
            }
            if itself.kind != 2 {
                fail(
                    13,
                    "fstatat with AT_SYMLINK_NOFOLLOW followed the link anyway",
                );
            }
            if itself.size != target.len() as u64 {
                fail(13, "a link's own size is not the length of its target");
            }
        }
        _ => fail(13, "fstatat could not stat a symbolic link"),
    }
    // A flag that genuinely cannot be honoured is still refused, not ignored.
    if fstatat(AT_FDCWD, &base, 0x1000).is_some() {
        fail(13, "fstatat accepted a flag it cannot honour");
    }

    close(dirfd);
    let _ = fs::remove_dir_all(&base);
    pass(
        13,
        "openat/mkdirat/unlinkat/fstatat resolve against a directory descriptor",
    );
}

fn test14(dir: &str) {
    use edos_lib::io::{mkdirat, symlink};
    use std::io::ErrorKind;

    let base = format!("{}/iotest_t14", dir);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir(&base).unwrap_or_else(|e| fail(14, &format!("create dir: {}", e)));

    fs::create_dir(format!("{}/sub", base))
        .unwrap_or_else(|e| fail(14, &format!("create subdir: {}", e)));
    fs::write(format!("{}/file", base), b"x")
        .unwrap_or_else(|e| fail(14, &format!("create file: {}", e)));
    // A dangling link still takes the name.
    if symlink("nowhere", &format!("{}/link", base)) != 0 {
        fail(14, "symlink failed");
    }

    let taken = ["sub", "file", "link"];
    for name in taken {
        match fs::create_dir(format!("{}/{}", base, name)) {
            Ok(()) => fail(14, &format!("mkdir over an existing {} succeeded", name)),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => fail(
                14,
                &format!(
                    "mkdir over {} gave {:?}, want AlreadyExists",
                    name,
                    e.kind()
                ),
            ),
        }
        if symlink("nowhere", &format!("{}/{}", base, name)) == 0 {
            fail(14, &format!("symlink over an existing {} succeeded", name));
        }
        if mkdirat(AT_FDCWD, &format!("{}/{}", base, name)) == 0 {
            fail(14, &format!("mkdirat over an existing {} succeeded", name));
        }
    }

    // The refused creates must not have left duplicate directory entries.
    let mut names: Vec<String> = fs::read_dir(&base)
        .unwrap_or_else(|e| fail(14, &format!("read_dir: {}", e)))
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    if names != ["file", "link", "sub"] {
        fail(
            14,
            &format!("directory holds {:?}, want one entry each", names),
        );
    }

    // O_CREAT without O_EXCL still opens a file that already exists.
    fs::write(format!("{}/file", base), b"yy")
        .unwrap_or_else(|e| fail(14, &format!("rewrite an existing file: {}", e)));
    if fs::read(format!("{}/file", base)).unwrap_or_default() != b"yy" {
        fail(14, "O_CREAT on an existing file lost the write");
    }

    let _ = fs::remove_dir_all(&base);
    pass(14, "creating over a taken name reports AlreadyExists");
}

// Test 15: a file ends at its size, not at the end of its last page
//
// A short file whose data was written through the page cache must not read
// back padded to 4096 bytes, and must not report a padded length.
fn test15(dir: &str) {
    let path = format!("{}/iotest_t15.bin", dir);
    let _ = fs::remove_file(&path);

    let body: Vec<u8> = (0..20).map(pattern).collect();
    fs::write(&path, &body).unwrap_or_else(|e| fail(15, &format!("write: {}", e)));

    let len = fs::metadata(&path)
        .unwrap_or_else(|e| fail(15, &format!("metadata: {}", e)))
        .len();
    if len != body.len() as u64 {
        fail(
            15,
            &format!("metadata says {} bytes, wrote {}", len, body.len()),
        );
    }

    let read_back = fs::read(&path).unwrap_or_else(|e| fail(15, &format!("read: {}", e)));
    if read_back != body {
        fail(
            15,
            &format!("read {} bytes, want {}", read_back.len(), body.len()),
        );
    }

    // Reading from the padding past EOF yields nothing, not a page of zeros.
    let f = File::open(&path).unwrap_or_else(|e| fail(15, &format!("open: {}", e)));
    let mut tail = [0u8; 64];
    let n = pread(f.as_raw_fd() as u64, &mut tail, body.len() as u64);
    if n != 0 {
        fail(15, &format!("pread past EOF returned {}, want 0", n));
    }
    drop(f);

    let _ = fs::remove_file(&path);
    pass(15, "file length is the file, not its last page");
}

// Test 16: a file grown by truncate is sparse, and its holes read as zeros
//
// Growing past the last allocated block leaves blocks nobody wrote. Reading
// one is not an error: it yields zeros, and writing into it later leaves the
// blocks around it untouched.
fn test16(dir: &str) {
    const GROWN: usize = 12_000;
    let path = format!("{}/iotest_t16.bin", dir);
    let _ = fs::remove_file(&path);

    let body: Vec<u8> = (0..20).map(pattern).collect();
    fs::write(&path, &body).unwrap_or_else(|e| fail(16, &format!("write: {}", e)));

    if truncate(&path, GROWN as u64) != 0 {
        fail(16, "truncate to grow failed");
    }

    let len = fs::metadata(&path)
        .unwrap_or_else(|e| fail(16, &format!("metadata: {}", e)))
        .len();
    if len != GROWN as u64 {
        fail(16, &format!("metadata says {} bytes, want {}", len, GROWN));
    }

    let grown = fs::read(&path).unwrap_or_else(|e| fail(16, &format!("read grown: {}", e)));
    if grown.len() != GROWN {
        fail(16, &format!("read {} bytes, want {}", grown.len(), GROWN));
    }
    if grown[..body.len()] != body[..] {
        fail(16, "grow lost the original contents");
    }
    if grown[body.len()..].iter().any(|&b| b != 0) {
        fail(16, "a hole did not read as zeros");
    }

    // A block entirely inside the hole reads as zeros too, and reads past the
    // new end still stop at it.
    let f = File::open(&path).unwrap_or_else(|e| fail(16, &format!("open: {}", e)));
    let mut mid = [0xAAu8; 512];
    let n = pread(f.as_raw_fd() as u64, &mut mid, 8192);
    if n != 512 || mid.iter().any(|&b| b != 0) {
        fail(16, &format!("pread inside the hole returned {}", n));
    }
    let n = pread(f.as_raw_fd() as u64, &mut mid, GROWN as u64);
    if n != 0 {
        fail(16, &format!("pread past EOF returned {}, want 0", n));
    }
    drop(f);

    // Filling one hole block leaves its neighbours zero.
    let f = OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(16, &format!("open for write: {}", e)));
    let filled = [0x5Au8; 256];
    if pwrite(f.as_raw_fd() as u64, &filled, 8192) != filled.len() as isize {
        fail(16, "pwrite into a hole failed");
    }
    drop(f);

    let after = fs::read(&path).unwrap_or_else(|e| fail(16, &format!("read after fill: {}", e)));
    if after.len() != GROWN {
        fail(16, &format!("fill changed the length to {}", after.len()));
    }
    if after[8192..8192 + filled.len()] != filled[..] {
        fail(16, "the byte written into the hole did not come back");
    }
    if after[..body.len()] != body[..] {
        fail(16, "filling a hole disturbed the first block");
    }
    if after[body.len()..8192].iter().any(|&b| b != 0)
        || after[8192 + filled.len()..].iter().any(|&b| b != 0)
    {
        fail(16, "filling a hole disturbed the blocks around it");
    }

    let _ = fs::remove_file(&path);
    pass(16, "a hole in a grown file reads as zeros");
}

// Test 17: renameat/symlinkat/readlinkat/faccessat take a directory descriptor
//
// The second tier of the `*at` family. Each one resolves its path the way
// `openat` does, and `renameat` names two descriptors at once.
fn test17(dir: &str) {
    use edos_lib::io::{
        AT_FDCWD, F_OK, W_OK, faccessat, fstatat, mkdirat, open, openat, readlinkat, renameat,
        symlinkat,
    };
    use edos_lib::process::close;

    let base = format!("{}/iotest_t17", dir);
    let _ = fs::remove_dir_all(&base);
    fs::create_dir(&base).unwrap_or_else(|e| fail(17, &format!("create dir: {}", e)));

    let dirfd = open(&base, 0);
    if dirfd < 0 {
        fail(17, "cannot open a directory as a descriptor");
    }

    let fd = openat(dirfd, "a", 0x40 | 1);
    if fd < 0 {
        fail(17, "openat with O_CREAT failed");
    }
    if pwrite(fd as u64, b"body", 0) != 4 {
        fail(17, "write through an openat descriptor failed");
    }
    close(fd as u64);

    // faccessat: a relative name resolves against the descriptor, an absolute
    // one ignores it, and a flag that cannot be honoured is refused.
    if faccessat(dirfd, "a", F_OK, 0) != 0 {
        fail(17, "faccessat cannot see a file in its own directory");
    }
    if faccessat(dirfd, "a", W_OK, 0) != 0 {
        fail(17, "faccessat denied write on a writable file");
    }
    if faccessat(dirfd, "missing", F_OK, 0) == 0 {
        fail(17, "faccessat found a file that does not exist");
    }
    if faccessat(AT_FDCWD, &base, F_OK, 0) != 0 {
        fail(17, "faccessat with AT_FDCWD cannot see an absolute path");
    }
    if faccessat(dirfd, "a", F_OK, 0x200) == 0 {
        fail(17, "faccessat accepted a flag it cannot honour");
    }

    // symlinkat puts the link where newdirfd says; the target is stored
    // verbatim and so resolves against the link's own directory.
    if symlinkat("a", dirfd, "link") != 0 {
        fail(17, "symlinkat failed");
    }
    if fs::read(format!("{}/link", base)).unwrap_or_default() != b"body" {
        fail(
            17,
            "reading through a symlinkat link gave the wrong contents",
        );
    }
    if symlinkat("a", dirfd, "link") == 0 {
        fail(17, "symlinkat created a second entry for a taken name");
    }

    let mut buf = [0u8; 16];
    let n = readlinkat(dirfd, "link", &mut buf);
    if n != 1 || &buf[..1] != b"a" {
        fail(17, &format!("readlinkat returned {} bytes, want 1", n));
    }
    // A buffer shorter than the target truncates rather than failing.
    if symlinkat("abcd", dirfd, "long") != 0 {
        fail(17, "symlinkat with a longer target failed");
    }
    let mut small = [0u8; 2];
    if readlinkat(dirfd, "long", &mut small) != 2 || &small != b"ab" {
        fail(17, "readlinkat did not truncate into a short buffer");
    }
    if readlinkat(dirfd, "a", &mut buf) != -1 {
        fail(17, "readlinkat read a file that is not a link");
    }

    // renameat: old and new resolve against their own descriptors.
    if mkdirat(dirfd, "sub") != 0 {
        fail(17, "mkdirat failed");
    }
    let subfd = open(&format!("{}/sub", base), 0);
    if subfd < 0 {
        fail(17, "cannot open the subdirectory as a descriptor");
    }
    if renameat(dirfd, "a", subfd, "moved") != 0 {
        fail(17, "renameat across two descriptors failed");
    }
    if fstatat(dirfd, "a", 0).is_some() {
        fail(17, "the old name survived renameat");
    }
    match fstatat(subfd, "moved", 0) {
        Some(st) if st.size == 4 => {}
        Some(st) => fail(17, &format!("renameat left {} bytes, want 4", st.size)),
        None => fail(17, "renameat did not create the new name"),
    }
    // An absolute path ignores its descriptor, and a closed one is refused.
    if renameat(AT_FDCWD, &format!("{}/sub/moved", base), dirfd, "back") != 0 {
        fail(17, "renameat with AT_FDCWD and an absolute path failed");
    }
    if fstatat(dirfd, "back", 0).is_none() {
        fail(17, "renameat back to the parent did not land");
    }
    if renameat(4242, "back", dirfd, "nope") != -1 {
        fail(17, "renameat accepted a closed descriptor");
    }

    close(subfd as u64);
    close(dirfd as u64);
    let _ = fs::remove_dir_all(&base);
    pass(
        17,
        "renameat/symlinkat/readlinkat/faccessat resolve against a directory descriptor",
    );
}

// Test 18: readv/writev move a list of buffers in one syscall
//
// The buffers are handled in order and each is filled or drained completely
// before the next, so a short transfer ends the sequence.
fn test18(dir: &str) {
    use edos_lib::io::{readv, writev};
    use std::io::{Seek, SeekFrom};

    let path = format!("{}/iotest_t18.dat", dir);
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap_or_else(|e| fail(18, &format!("create: {}", e)));
    let fd = file.as_raw_fd() as u64;

    // An empty buffer contributes nothing and does not end the sequence.
    let parts: [&[u8]; 4] = [b"alpha", b"", b"-beta", b"-gamma"];
    let n = writev(fd, &parts);
    if n != 16 {
        fail(18, &format!("writev wrote {} bytes, want 16", n));
    }
    if writev(fd, &[]) != 0 {
        fail(18, "writev of no buffers did not return 0");
    }
    file.sync_all()
        .unwrap_or_else(|e| fail(18, &format!("sync: {}", e)));
    match fs::read(&path) {
        Ok(got) if got == b"alpha-beta-gamma" => {}
        Ok(got) => fail(18, &format!("file holds {:?}, want alpha-beta-gamma", got)),
        Err(e) => fail(18, &format!("read back: {}", e)),
    }

    // readv fills in the same order, through the descriptor's own offset.
    file.seek(SeekFrom::Start(0))
        .unwrap_or_else(|e| fail(18, &format!("seek: {}", e)));
    let (mut a, mut b, mut c) = ([0u8; 5], [0u8; 5], [0u8; 6]);
    {
        let mut bufs: [&mut [u8]; 3] = [&mut a, &mut b, &mut c];
        let n = readv(fd, &mut bufs);
        if n != 16 {
            fail(18, &format!("readv read {} bytes, want 16", n));
        }
    }
    if &a != b"alpha" || &b != b"-beta" || &c != b"-gamma" {
        fail(18, "readv scattered the bytes into the wrong buffers");
    }

    // A short read ends the sequence: three bytes remain, so the second buffer
    // takes one of its four and the read stops there.
    file.seek(SeekFrom::Start(13))
        .unwrap_or_else(|e| fail(18, &format!("seek: {}", e)));
    let (mut head, mut tail) = ([0u8; 2], [0u8; 4]);
    {
        let mut bufs: [&mut [u8]; 2] = [&mut head, &mut tail];
        let n = readv(fd, &mut bufs);
        if n != 3 {
            fail(18, &format!("readv near EOF read {} bytes, want 3", n));
        }
    }
    if &head != b"mm" || tail[0] != b'a' || tail[1..] != [0, 0, 0] {
        fail(18, "a short readv did not stop at the end of the file");
    }

    // At EOF there is nothing to fill, which is 0 rather than an error.
    let mut end = [0u8; 8];
    {
        let mut bufs: [&mut [u8]; 1] = [&mut end];
        if readv(fd, &mut bufs) != 0 {
            fail(18, "readv at EOF did not return 0");
        }
    }

    // More buffers than IOV_MAX is refused outright.
    let too_many: Vec<&[u8]> = vec![&b"x"[..]; 1025];
    if writev(fd, &too_many) != -1 {
        fail(18, "writev accepted more buffers than IOV_MAX");
    }

    drop(file);
    let _ = fs::remove_file(&path);
    pass(18, "readv/writev move a buffer list in order");
}

// -----------------------------------------------------------------------
// Test 19: the std surface that used to report "unsupported"
// -----------------------------------------------------------------------
//
// Every other test here calls `edos_lib`, which reaches the kernel directly.
// This one goes through `std` on purpose: the wrappers worked for years while
// the same operations were `unsupported()` one layer up, and that gap is
// exactly what nothing was testing.
fn test19(dir: &str) {
    use std::io::{IoSlice, IoSliceMut, Read, Seek, SeekFrom, Write};
    use std::time::{Duration, Instant, SystemTime};

    let path = format!("{}/iotest_t19.dat", dir);
    let link = format!("{}/iotest_t19.link", dir);
    let _ = fs::remove_file(&link);
    fs::write(&path, b"nineteen").unwrap_or_else(|e| fail(19, &format!("create: {}", e)));

    // Symbolic links, through std rather than through the syscall wrapper.
    #[allow(deprecated)]
    fs::soft_link(&path, &link).unwrap_or_else(|e| fail(19, &format!("soft_link: {}", e)));
    match fs::read_link(&link) {
        Ok(target) if target.to_string_lossy() == path => {}
        Ok(target) => fail(19, &format!("read_link gave {:?}, want {}", target, path)),
        Err(e) => fail(19, &format!("read_link: {}", e)),
    }
    match fs::read(&link) {
        Ok(got) if got == b"nineteen" => {}
        Ok(_) => fail(19, "reading through the link gave the wrong bytes"),
        Err(e) => fail(19, &format!("read through link: {}", e)),
    }

    // Timestamps: `stat` always carried them, `Metadata` used to refuse them.
    let times = fs::FileTimes::new()
        .set_accessed(SystemTime::UNIX_EPOCH + Duration::from_secs(1_300_000_000))
        .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_400_000_000));
    let file = OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(19, &format!("open for set_times: {}", e)));
    file.set_times(times)
        .unwrap_or_else(|e| fail(19, &format!("File::set_times: {}", e)));
    drop(file);
    let meta = fs::metadata(&path).unwrap_or_else(|e| fail(19, &format!("metadata: {}", e)));
    let modified = meta
        .modified()
        .unwrap_or_else(|e| fail(19, &format!("Metadata::modified: {}", e)));
    let secs = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_else(|e| fail(19, &format!("modified before the epoch: {}", e)))
        .as_secs();
    if secs != 1_400_000_000 {
        fail(19, &format!("modified is {}, want 1400000000", secs));
    }

    // `read_dir` streams now; it still has to find what it is asked for.
    let entries = fs::read_dir(dir).unwrap_or_else(|e| fail(19, &format!("read_dir: {}", e)));
    let mut found = false;
    let mut link_is_symlink = false;
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| fail(19, &format!("read_dir entry: {}", e)));
        if entry.file_name() == std::ffi::OsStr::new("iotest_t19.dat") {
            found = true;
        }
        if entry.file_name() == std::ffi::OsStr::new("iotest_t19.link") {
            link_is_symlink = entry
                .file_type()
                .unwrap_or_else(|e| fail(19, &format!("file_type: {}", e)))
                .is_symlink();
        }
    }
    if !found {
        fail(19, "read_dir did not list a file it had just created");
    }
    if !link_is_symlink {
        fail(19, "read_dir reported the symlink as an ordinary file");
    }

    // Vectored I/O through `Write`/`Read`, which used to report itself
    // unavailable and fall back to one buffer at a time.
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap_or_else(|e| fail(19, &format!("reopen: {}", e)));
    if !file.is_write_vectored() {
        fail(19, "File still reports no vectored write");
    }
    let n = file
        .write_vectored(&[IoSlice::new(b"one-"), IoSlice::new(b"two")])
        .unwrap_or_else(|e| fail(19, &format!("write_vectored: {}", e)));
    if n != 7 {
        fail(19, &format!("write_vectored wrote {}, want 7", n));
    }
    file.seek(SeekFrom::Start(0))
        .unwrap_or_else(|e| fail(19, &format!("seek: {}", e)));
    let (mut head, mut tail) = ([0u8; 4], [0u8; 3]);
    let n = file
        .read_vectored(&mut [IoSliceMut::new(&mut head), IoSliceMut::new(&mut tail)])
        .unwrap_or_else(|e| fail(19, &format!("read_vectored: {}", e)));
    if n != 7 || &head != b"one-" || &tail != b"two" {
        fail(19, "read_vectored filled the buffers wrongly");
    }
    drop(file);

    // `try_exists` is `access` now, and still has to answer correctly.
    if !std::path::Path::new(&path).try_exists().unwrap_or(false) {
        fail(19, "try_exists denied a file that exists");
    }
    let missing = format!("{}/iotest_t19_missing.dat", dir);
    if std::path::Path::new(&missing).try_exists().unwrap_or(true) {
        fail(19, "try_exists claimed a missing file exists");
    }

    // A sub-millisecond sleep used to round to zero and return at once.
    let start = Instant::now();
    std::thread::sleep(Duration::from_micros(200));
    if start.elapsed() < Duration::from_micros(200) {
        fail(19, "thread::sleep returned before the time it was given");
    }

    let _ = fs::remove_file(&link);
    let _ = fs::remove_file(&path);
    pass(
        19,
        "std reaches symlinks, file times, streaming read_dir, vectored I/O, \
         access and nanosleep",
    );
}

// -----------------------------------------------------------------------
// Test 20: a zero-length transfer still answers for its descriptor
// -----------------------------------------------------------------------
fn test20(dir: &str) {
    // Userspace probes a descriptor with a transfer of no bytes, so a call
    // that answers 0 without looking at the fd reports a success it never had.
    const CLOSED: u64 = 9999;
    let empty: [u8; 0] = [];
    let mut empty_out: [u8; 0] = [];

    let cases: [(&str, isize); 6] = [
        ("read", edos_lib::io::sys_read(CLOSED, &mut empty_out)),
        ("write", process::write(CLOSED, &empty)),
        ("pread", pread(CLOSED, &mut empty_out, 0)),
        ("pwrite", pwrite(CLOSED, &empty, 0)),
        ("readv", edos_lib::io::readv(CLOSED, &mut [])),
        ("writev", edos_lib::io::writev(CLOSED, &[])),
    ];
    for (name, ret) in cases {
        if ret >= 0 {
            fail(
                20,
                &format!(
                    "{}(closed fd, 0 bytes) returned {}, want failure",
                    name, ret
                ),
            );
        }
    }

    // The same calls on a descriptor that is open still transfer nothing and
    // report success, so the check above cannot be satisfied by failing them all.
    let path = format!("{}/iotest_t20.dat", dir);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap_or_else(|e| fail(20, &format!("create: {}", e)));
    let fd = file.as_raw_fd() as u64;

    let cases: [(&str, isize); 6] = [
        ("read", edos_lib::io::sys_read(fd, &mut empty_out)),
        ("write", process::write(fd, &empty)),
        ("pread", pread(fd, &mut empty_out, 0)),
        ("pwrite", pwrite(fd, &empty, 0)),
        ("readv", edos_lib::io::readv(fd, &mut [])),
        ("writev", edos_lib::io::writev(fd, &[])),
    ];
    for (name, ret) in cases {
        if ret != 0 {
            fail(
                20,
                &format!("{}(open fd, 0 bytes) returned {}, want 0", name, ret),
            );
        }
    }

    drop(file);
    let _ = fs::remove_file(&path);
    pass(
        20,
        "a zero-length transfer reports EBADF on a closed descriptor",
    );
}

/// Whether `fd` would accept a write right now, asked without writing.
fn writable(fd: u64) -> bool {
    let mut fds = [edos_lib::io::SelectFd {
        fd,
        interests: edos_lib::io::PollState {
            writable: true,
            ..Default::default()
        },
        result: Default::default(),
    }];
    edos_lib::io::poll(&mut fds, 0);
    fds[0].result.writable
}

/// A terminal holds a bounded amount of output, and says so.
///
/// Without the bound a program that outruns the terminal drawing for it grows
/// the kernel heap without limit, which is what this used to do: the ring was
/// the same `ByteRing` a pipe used before pipes were bounded. The observable is
/// `poll`, because it is the one that cannot hang the test: a blocking write
/// into a full terminal is exactly the thing being checked for, so asking
/// whether the write *would* block is the only safe way to ask.
fn test21() {
    let Some((master, slave)) = process::openpty() else {
        fail(21, "openpty failed");
    };

    // Nothing ever reads `master`, so every byte written here stays in the
    // kernel. 1 MiB is far past any defensible capacity for a terminal.
    const CAP: usize = 1024 * 1024;
    const CHUNK: usize = 4096;
    let chunk = [b'x'; CHUNK];
    let mut accepted = 0usize;
    while accepted < CAP {
        if !writable(slave) {
            break;
        }
        let n = process::write(slave, &chunk);
        if n <= 0 {
            fail(21, &format!("write to the slave returned {}", n));
        }
        accepted += n as usize;
    }

    if accepted >= CAP {
        fail(
            21,
            &format!("the terminal took {} bytes with nobody reading", accepted),
        );
    }
    if writable(slave) {
        fail(21, "a full terminal still reports itself writable");
    }

    // Draining the master must give the room back, or a writer parked on this
    // would never wake: the bound has to be a wait, not a wall.
    let mut sink = [0u8; CHUNK];
    let drained = edos_lib::io::sys_read(master, &mut sink);
    if drained <= 0 {
        fail(21, &format!("read from the master returned {}", drained));
    }
    if !writable(slave) {
        fail(21, "a drained terminal still reports itself full");
    }

    close(master);
    close(slave);
    pass(
        21,
        &format!("a terminal bounds its output at {} bytes", accepted),
    );
}

/// A named pipe is a rendezvous: opening one end waits for the other, and what
/// one end writes the other reads.
///
/// The point of the test is the pair of programs with no common parent, which
/// is what a FIFO buys over an anonymous pipe. A thread stands in for the
/// second program: it opens by name, which is the part that has to work.
fn test22(dir: &str) {
    let path = format!("{}/iotest_t22.fifo", dir);
    let _ = fs::remove_file(&path);
    if edos_lib::io::mkfifo(&path) < 0 {
        fail(22, &format!("mkfifo: {:?}", edos_lib::io::last_errno()));
    }

    // It is a FIFO to `stat`, not a regular file: a kind reported wrong is how
    // a shell ends up truncating one instead of opening it.
    let info = stat(&path).unwrap_or_else(|| fail(22, "stat on the fifo failed"));
    if info.kind != 4 {
        fail(
            22,
            &format!("stat reports kind {}, want 4 (fifo)", info.kind),
        );
    }

    const MESSAGE: &[u8] = b"through the name\n";
    let writer_path = path.clone();
    let writer = thread::spawn(move || {
        // Blocks here until the reader below opens its end.
        let fd = open(&writer_path, edos_lib::io::O_WRONLY);
        if fd < 0 {
            return -1i64;
        }
        let n = process::write(fd as u64, MESSAGE);
        close(fd as u64);
        n as i64
    });

    let fd = open(&path, edos_lib::io::O_RDONLY);
    if fd < 0 {
        fail(
            22,
            &format!("open for reading: {:?}", edos_lib::io::last_errno()),
        );
    }
    let mut buf = [0u8; 64];
    let n = edos_lib::io::sys_read(fd as u64, &mut buf);
    if n <= 0 {
        fail(22, &format!("read from the fifo returned {}", n));
    }
    if &buf[..n as usize] != MESSAGE {
        fail(
            22,
            &format!("read {:?}, want {:?}", &buf[..n as usize], MESSAGE),
        );
    }

    // With the writer gone the read reports end of file rather than waiting,
    // which is what tells a reader the transfer is over.
    let eof = edos_lib::io::sys_read(fd as u64, &mut buf);
    if eof != 0 {
        fail(
            22,
            &format!("after the writer closed, read returned {}", eof),
        );
    }
    close(fd as u64);

    let written = writer.join().unwrap_or(-1);
    if written != MESSAGE.len() as i64 {
        fail(22, &format!("the writer reported {} bytes", written));
    }

    // Opening a FIFO for writing with nobody reading is ENXIO, not a wait: it
    // is what lets a control client fail instead of hanging on a dead server.
    let refused = open(&path, edos_lib::io::O_WRONLY | edos_lib::io::O_NONBLOCK);
    if refused >= 0 {
        close(refused as u64);
        fail(
            22,
            "a non-blocking write-only open succeeded with no reader",
        );
    }

    let _ = fs::remove_file(&path);
    pass(22, "a named pipe rendezvous carried a message end to end");
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
    test13(dir);
    test14(dir);
    test15(dir);
    test16(dir);
    test17(dir);
    test18(dir);
    test19(dir);
    test20(dir);
    test21();
    test22(dir);
    println!("iotest: all tests passed [{}]", dir);
    std::process::exit(0);
}
