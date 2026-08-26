use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;
use std::time::Instant;

use edos_lib::mem::{
    MAP_ANONYMOUS, MAP_PRIVATE, MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE, mmap, mprotect, msync,
    munmap,
};
use edos_lib::process;

const PAGE: u64 = 4096;

fn fail(test: u32, dir: &str, msg: &str) -> ! {
    eprintln!("FAIL test {} [{}]: {}", test, dir, msg);
    std::process::exit(1);
}

fn pass(test: u32, dir: &str, detail: &str) {
    println!("PASS test {} [{}]: {}", test, dir, detail);
}

/// Where a child leaves the address it is about to touch.
///
/// A case that only asserts the child died with code 11 passes on *any* fault,
/// including one at an address the case knows nothing about, so it can report
/// that the kernel rejected a past-EOF read while the child in fact took a
/// null dereference on the way there. The child writes down what it is about
/// to touch and the parent checks it against its own pointer, which turns the
/// exit code into a statement about this mapping.
fn touch_record(test: u32, dir: &str) -> String {
    format!("{}/mmaptest_t{}.touch", dir, test)
}

/// Child side: record `addr` before the access that is expected to fault.
fn record_touch(test: u32, dir: &str, addr: *const u8) {
    let path = touch_record(test, dir);
    if let Err(e) = fs::write(&path, format!("{}", addr as usize)) {
        println!("test{test} child: could not record {path}: {e}");
    }
}

/// Parent side: the address the child died on is the one this case is about.
fn check_touch(test: u32, dir: &str, expected: *const u8) {
    let path = touch_record(test, dir);
    let recorded = match fs::read(&path).map(|b| String::from_utf8_lossy(&b).into_owned()) {
        Ok(s) => s,
        Err(e) => fail(
            test,
            dir,
            &format!("child left no record of the address it touched: {}", e),
        ),
    };
    let _ = fs::remove_file(&path);

    let got: usize = match recorded.trim().parse() {
        Ok(v) => v,
        Err(e) => fail(
            test,
            dir,
            &format!("unreadable touch record {recorded:?}: {e}"),
        ),
    };
    if got != expected as usize {
        fail(
            test,
            dir,
            &format!(
                "child was about to touch {:#x} where the parent holds {:#x}, so it died of \
                 something other than this case",
                got, expected as usize
            ),
        );
    }
}

fn timed<R>(test: u32, dir: &str, label: &str, f: impl FnOnce() -> R) -> R {
    let start = Instant::now();
    let r = f();
    let us = start.elapsed().as_micros();
    if us >= 10_000 {
        println!("  [{}] test {} {} took {} ms", dir, test, label, us / 1000);
    } else {
        println!("  [{}] test {} {} took {} us", dir, test, label, us);
    }
    r
}

// -----------------------------------------------------------------------
// Test 1: MAP_PRIVATE read -- verify mmap bytes match fs::read bytes
// -----------------------------------------------------------------------
fn test1(dir: &str) {
    let path = format!("{}/mmaptest_t1.dat", dir);
    // Full-page content. Kernel requires page-aligned mmap length for
    // file-backed mappings.
    let mut content = vec![0u8; PAGE as usize];
    let hello = b"Hello, mmap world!  ";
    content[..hello.len()].copy_from_slice(hello);
    fs::write(&path, &content).unwrap_or_else(|e| fail(1, dir, &format!("write file: {}", e)));

    let expected: Vec<u8> =
        fs::read(&path).unwrap_or_else(|e| fail(1, dir, &format!("fs::read: {}", e)));

    let file = File::open(&path).unwrap_or_else(|e| fail(1, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(core::ptr::null_mut(), PAGE, PROT_READ, MAP_PRIVATE, fd, 0)
        .unwrap_or_else(|e| fail(1, dir, &format!("mmap: {e:?}")))
        .as_ptr();

    let mapped: Vec<u8> = unsafe { core::slice::from_raw_parts(ptr, PAGE as usize).to_vec() };
    let _ = munmap(ptr, PAGE);

    if mapped != expected {
        fail(
            1,
            dir,
            &format!(
                "mmap first 20 {:?} != fs::read first 20 {:?}",
                &mapped[..20],
                &expected[..20]
            ),
        );
    }

    pass(
        1,
        dir,
        &format!(
            "first 20 bytes via mmap match fs::read: {:?}",
            &mapped[..20]
        ),
    );
}

// -----------------------------------------------------------------------
// Test 2: MAP_PRIVATE COW write -- private write does NOT reach disk
// -----------------------------------------------------------------------
fn test2(dir: &str) {
    let path = format!("{}/mmaptest_t2.dat", dir);
    let content = vec![b'A'; PAGE as usize];
    fs::write(&path, &content).unwrap_or_else(|e| fail(2, dir, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .open(&path)
        .unwrap_or_else(|e| fail(2, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE,
        fd,
        0,
    )
    .unwrap_or_else(|e| fail(2, dir, &format!("mmap: {e:?}")))
    .as_ptr();

    // Write 'B' via private mapping
    unsafe { ptr.write(b'B') };
    let mapped_byte = unsafe { ptr.read() };
    if mapped_byte != b'B' {
        fail(
            2,
            dir,
            &format!("expected 'B' in mapping, got {}", mapped_byte),
        );
    }

    let _ = munmap(ptr, PAGE);

    // File on disk must still start with 'A'
    let disk_byte = fs::read(&path).unwrap_or_else(|e| fail(2, dir, &format!("re-read: {}", e)))[0];
    if disk_byte != b'A' {
        fail(
            2,
            dir,
            &format!(
                "COW failed: disk byte should be 'A' but got '{}'",
                disk_byte as char
            ),
        );
    }

    pass(2, dir, "COW write visible in mapping ('B'), disk still 'A'");
}

// -----------------------------------------------------------------------
// Test 3: MAP_SHARED write + msync -- write must reach disk after msync
// -----------------------------------------------------------------------
fn test3(dir: &str) {
    let path = format!("{}/mmaptest_t3.dat", dir);
    let content = vec![b'A'; PAGE as usize];
    fs::write(&path, &content).unwrap_or_else(|e| fail(3, dir, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(3, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0,
    )
    .unwrap_or_else(|e| fail(3, dir, &format!("mmap: {e:?}")))
    .as_ptr();

    unsafe { ptr.write(b'C') };

    if let Err(e) = unsafe { msync(ptr, PAGE, MS_SYNC) } {
        fail(3, dir, &format!("msync failed: {e:?}"));
    }

    let _ = munmap(ptr, PAGE);

    let disk_byte = fs::read(&path).unwrap_or_else(|e| fail(3, dir, &format!("re-read: {}", e)))[0];
    if disk_byte != b'C' {
        fail(
            3,
            dir,
            &format!(
                "expected 'C' on disk after msync, got '{}'",
                disk_byte as char
            ),
        );
    }

    pass(3, dir, "MAP_SHARED write + msync: disk byte is 'C'");
}

// -----------------------------------------------------------------------
// Test 4: MAP_SHARED two-mapper visibility within one process
// -----------------------------------------------------------------------
fn test4(dir: &str) {
    let path = format!("{}/mmaptest_t4.dat", dir);
    let content = vec![b'A'; PAGE as usize];
    fs::write(&path, &content).unwrap_or_else(|e| fail(4, dir, &format!("write file: {}", e)));

    let file_a = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(4, dir, &format!("open A: {}", e)));
    let file_b = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(4, dir, &format!("open B: {}", e)));

    let fd_a = file_a.as_raw_fd();
    let fd_b = file_b.as_raw_fd();

    let ptr_a = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd_a,
        0,
    )
    .unwrap_or_else(|e| fail(4, dir, &format!("mmap A: {e:?}")))
    .as_ptr();
    let ptr_b = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd_b,
        0,
    )
    .unwrap_or_else(|e| fail(4, dir, &format!("mmap B: {e:?}")))
    .as_ptr();

    // Write 'D' via mapping A
    unsafe { ptr_a.write(b'D') };

    // Read via mapping B -- must see 'D' (same page cache frame)
    let seen = unsafe { ptr_b.read() };

    let _ = munmap(ptr_a, PAGE);
    let _ = munmap(ptr_b, PAGE);

    if seen != b'D' {
        fail(
            4,
            dir,
            &format!(
                "two-mapper visibility failed: expected 'D' via mapping B, got '{}'",
                seen as char
            ),
        );
    }

    pass(
        4,
        dir,
        "write via mapping A visible via mapping B without msync",
    );
}

// -----------------------------------------------------------------------
// Test 5: Past-EOF fault kills child (fork + deref second page of 4KB file)
// -----------------------------------------------------------------------
fn test5(dir: &str) {
    let path = format!("{}/mmaptest_t5.dat", dir);
    let content = vec![b'X'; PAGE as usize]; // 4 KB
    fs::write(&path, &content).unwrap_or_else(|e| fail(5, dir, &format!("write file: {}", e)));

    let file = File::open(&path).unwrap_or_else(|e| fail(5, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    // Map 2 pages even though file is only 1 page
    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE * 2,
        PROT_READ,
        MAP_PRIVATE,
        fd,
        0,
    )
    .unwrap_or_else(|e| fail(5, dir, &format!("mmap: {e:?}")))
    .as_ptr();

    // First page (in-file) must be readable
    let first = unsafe { ptr.read() };
    if first != b'X' {
        fail(5, dir, &format!("expected 'X' at byte 0, got {}", first));
    }

    // Fork a child; the child accesses the second page (past EOF), which must
    // trigger a kill (exit code 11 in EDOS).
    let child_pid = process::fork().unwrap_or_else(|e| {
        let _ = munmap(ptr, PAGE * 2);
        fail(5, dir, &format!("fork failed: {e:?}"))
    });

    if child_pid == 0 {
        // Child: touch the past-EOF page -- kernel must kill us.
        // read_volatile prevents the compiler from optimizing out the load.
        record_touch(5, dir, unsafe { ptr.add(PAGE as usize) });
        let byte = unsafe { core::ptr::read_volatile(ptr.add(PAGE as usize)) };
        // Prints only if the access somehow didn't fault.
        println!("test5 child: unexpected byte {} past EOF", byte);
        std::process::exit(0);
    }

    // Parent: wait for the child
    let exit_code = process::waitpid(child_pid);
    let _ = munmap(ptr, PAGE * 2);

    if exit_code == 11 {
        check_touch(5, dir, unsafe { ptr.add(PAGE as usize) });
        pass(5, dir, "past-EOF fault killed child with code 11: ok");
    } else {
        fail(
            5,
            dir,
            &format!(
                "expected child exit code 11 (SIGSEGV-equiv), got {}",
                exit_code
            ),
        );
    }
}

// -----------------------------------------------------------------------
// Test 6: Truncate post-mapping page kills child
// -----------------------------------------------------------------------
fn test6(dir: &str) {
    let path = format!("{}/mmaptest_t6.dat", dir);
    let content = vec![b'Y'; PAGE as usize * 2]; // 8 KB
    fs::write(&path, &content).unwrap_or_else(|e| fail(6, dir, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(6, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE * 2,
        PROT_READ,
        MAP_SHARED,
        fd,
        0,
    )
    .unwrap_or_else(|e| fail(6, dir, &format!("mmap: {e:?}")))
    .as_ptr();

    // Both pages accessible before truncation
    let b0 = unsafe { ptr.read() };
    let b1 = unsafe { ptr.add(PAGE as usize).read() };
    if b0 != b'Y' || b1 != b'Y' {
        fail(6, dir, "pre-truncate read failed");
    }

    // Truncate to 4 KB
    file.set_len(PAGE)
        .unwrap_or_else(|e| fail(6, dir, &format!("set_len: {}", e)));

    // Fork a child to access the now-truncated second page
    let child_pid = process::fork().unwrap_or_else(|e| {
        let _ = munmap(ptr, PAGE * 2);
        fail(6, dir, &format!("fork failed: {e:?}"))
    });

    if child_pid == 0 {
        // Child: access byte 4096 (past truncated end) -- should be killed.
        // read_volatile prevents the compiler from optimizing out the load.
        record_touch(6, dir, unsafe { ptr.add(PAGE as usize) });
        let byte = unsafe { core::ptr::read_volatile(ptr.add(PAGE as usize)) };
        println!("test6 child: unexpected byte {} after truncate", byte);
        std::process::exit(0);
    }

    let exit_code = process::waitpid(child_pid);
    let _ = munmap(ptr, PAGE * 2);

    if exit_code == 11 {
        check_touch(6, dir, unsafe { ptr.add(PAGE as usize) });
        pass(6, dir, "post-truncate fault killed child with code 11: ok");
    } else {
        fail(
            6,
            dir,
            &format!(
                "expected child exit code 11 after truncate, got {}",
                exit_code
            ),
        );
    }
}

// -----------------------------------------------------------------------
// Test 7: fsync + msync round-trip
// -----------------------------------------------------------------------
fn test7(dir: &str) {
    let path = format!("{}/mmaptest_t7.dat", dir);
    let content = vec![b'A'; PAGE as usize];
    fs::write(&path, &content).unwrap_or_else(|e| fail(7, dir, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(7, dir, &format!("open rw: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0,
    )
    .unwrap_or_else(|e| fail(7, dir, &format!("mmap: {e:?}")))
    .as_ptr();

    unsafe { ptr.write(b'Z') };

    if let Err(e) = unsafe { msync(ptr, PAGE, MS_SYNC) } {
        fail(7, dir, &format!("msync failed: {e:?}"));
    }

    let _ = munmap(ptr, PAGE);

    // fsync via std (file is still open)
    file.sync_all()
        .unwrap_or_else(|e| fail(7, dir, &format!("sync_all: {}", e)));
    drop(file);

    // Re-open and read
    let disk_byte = fs::read(&path).unwrap_or_else(|e| fail(7, dir, &format!("re-read: {}", e)))[0];
    if disk_byte != b'Z' {
        fail(
            7,
            dir,
            &format!(
                "expected 'Z' on disk after msync+fsync, got '{}'",
                disk_byte as char
            ),
        );
    }

    pass(7, dir, "fsync + msync round-trip: byte 0 is 'Z'");
}

// -----------------------------------------------------------------------
// Test 8: Fork + MAP_PRIVATE COW isolation
//
// The child writes to its private mapping. The parent's view must be
// unchanged and the file on disk must be unchanged. Exercises the
// FileBacked-specific COW-across-fork path.
// -----------------------------------------------------------------------
fn test8(dir: &str) {
    let path = format!("{}/mmaptest_t8.dat", dir);
    let content = vec![b'A'; PAGE as usize];
    fs::write(&path, &content).unwrap_or_else(|e| fail(8, dir, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(8, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE,
        fd,
        0,
    )
    .unwrap_or_else(|e| fail(8, dir, &format!("mmap: {e:?}")))
    .as_ptr();

    let child_pid = process::fork().unwrap_or_else(|e| {
        let _ = munmap(ptr, PAGE);
        fail(8, dir, &format!("fork failed: {e:?}"))
    });

    if child_pid == 0 {
        // Child: write to its private mapping. COW should isolate the
        // parent's view and not touch the on-disk file.
        record_touch(8, dir, ptr);
        unsafe { core::ptr::write_volatile(ptr, b'X') };
        let mine = unsafe { core::ptr::read_volatile(ptr) };
        if mine != b'X' {
            println!("test8 child: child saw {} after its own write", mine);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    let exit_code = process::waitpid(child_pid);
    if exit_code != 0 {
        let _ = munmap(ptr, PAGE);
        // The child is not supposed to fault here at all, so say which address
        // it was working from: a child holding an address its parent does not
        // is a different bug from one the kernel refused to fill.
        let held = fs::read(touch_record(8, dir))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|_| "?".into());
        fail(
            8,
            dir,
            &format!(
                "child exited {}, expected 0 (child held {}, parent holds {})",
                exit_code,
                held.trim(),
                ptr as usize
            ),
        );
    }
    let _ = fs::remove_file(touch_record(8, dir));

    // Parent view: byte 0 must still be 'A' (COW isolation).
    let parent_byte = unsafe { core::ptr::read_volatile(ptr) };
    let _ = munmap(ptr, PAGE);
    if parent_byte != b'A' {
        fail(
            8,
            dir,
            &format!("parent saw {} after child write, expected 'A'", parent_byte),
        );
    }

    // Disk must be untouched too.
    let on_disk: Vec<u8> =
        fs::read(&path).unwrap_or_else(|e| fail(8, dir, &format!("fs::read: {}", e)));
    if on_disk[0] != b'A' {
        fail(
            8,
            dir,
            &format!("disk byte 0 is {}, expected 'A'", on_disk[0]),
        );
    }

    pass(
        8,
        dir,
        "fork + MAP_PRIVATE: parent view isolated, disk unchanged",
    );
}

// -----------------------------------------------------------------------
// Test 9: Unlink-while-MAP_SHARED-dirty
//
// Open a file, map MAP_SHARED, write into the mapping, then unlink the
// file while the mapping is still live. The kernel must NOT panic; the
// pin counter holds the cluster chain (FAT32) or node (memfs/EFS) alive
// until munmap. After munmap the path must not be accessible.
//
// Skipped on filesystems that do not support page-cache mmap (MAP_FAILED
// on mmap is the observable signal for those, since they reject the mmap
// syscall).
// -----------------------------------------------------------------------
fn test9(dir: &str) {
    let path = format!("{}/mmaptest_t9.dat", dir);

    // Write 8 KB of 'U'
    let content = vec![b'U'; PAGE as usize * 2];
    fs::write(&path, &content).unwrap_or_else(|e| fail(9, dir, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|e| fail(9, dir, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    // A filesystem without page-cache mmap is a skip, not a failure.
    let Ok(ptr) = mmap(
        core::ptr::null_mut(),
        PAGE * 2,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0,
    ) else {
        drop(file);
        // Clean up the file we created; ignore errors (file may be gone).
        let _ = fs::remove_file(&path);
        println!("SKIP test 9 [{}]: mmap not supported on this fs", dir);
        return;
    };
    let ptr = ptr.as_ptr();

    // Write 'V' at offset 0 through the mapping while the file is still linked.
    unsafe { core::ptr::write_volatile(ptr, b'V') };

    // Unlink the file. The kernel must keep the data alive because the mapping
    // pins the inode.
    fs::remove_file(&path).unwrap_or_else(|e| fail(9, dir, &format!("remove_file: {}", e)));

    // The mapping must still read 'V' after unlink (inode is pinned).
    let still_v = unsafe { core::ptr::read_volatile(ptr) };
    if still_v != b'V' {
        let _ = munmap(ptr, PAGE * 2);
        fail(
            9,
            dir,
            &format!(
                "mapping read '{}' after unlink, expected 'V'",
                still_v as char
            ),
        );
    }

    // msync the dirty page -- flush_page must succeed even for orphaned inode.
    if let Err(e) = unsafe { msync(ptr, PAGE * 2, MS_SYNC) } {
        let _ = munmap(ptr, PAGE * 2);
        fail(9, dir, &format!("msync after unlink failed: {e:?}"));
    }

    let _ = munmap(ptr, PAGE * 2);
    drop(file);

    // After munmap (last unpin), the path must not be accessible.
    match fs::read(&path) {
        Err(_) => {}
        Ok(_) => fail(
            9,
            dir,
            "re-open after unlink+munmap succeeded (should fail)",
        ),
    }

    pass(
        9,
        dir,
        "unlink-while-mapped: mapping stayed live, path gone after munmap",
    );
}

// -----------------------------------------------------------------------
// Test 10: Exec from cached fs
//
// Copy a binary to the test dir (only meaningful under memfs /tmp or EFS
// /var), exec it, and verify it runs successfully. Exercises the ELF
// loader's PageCacheOps path for memfs.
//
// Only runs when dir is /tmp or /var (skipped elsewhere).
// -----------------------------------------------------------------------
fn test10(dir: &str) {
    // Only meaningful for /tmp (memfs) and /var (EFS) where exec makes sense.
    // FAT32 and other mounts may not have writable /bin.
    if dir != "/tmp" && dir != "/var" {
        println!(
            "SKIP test 10 [{}]: exec test only runs on /tmp and /var",
            dir
        );
        return;
    }

    let dst = format!("{}/mmaptest_echo", dir);

    // Copy /bin/echo to the test dir.
    let src_size = fs::metadata("/bin/echo").map(|m| m.len()).unwrap_or(0);
    let copied = timed(10, dir, "fs::copy(/bin/echo)", || {
        fs::copy("/bin/echo", &dst)
    })
    .unwrap_or_else(|e| fail(10, dir, &format!("copy /bin/echo -> {}: {}", dst, e)));
    println!(
        "  [{}] test 10 copied {} bytes (src size {})",
        dir, copied, src_size
    );

    // Spawn the copy and wait for it to exit cleanly.
    let pid = timed(10, dir, "spawn+wait", || {
        let pid = process::spawn(&dst, &["hello from test10"], 0, 1, 2).unwrap_or_else(|e| {
            let _ = fs::remove_file(&dst);
            fail(10, dir, &format!("spawn {dst}: {e:?}"))
        });
        let exit_code = process::waitpid(pid);
        (pid, exit_code)
    });
    let (_, exit_code) = pid;

    // Clean up regardless of result.
    let _ = fs::remove_file(&dst);

    if exit_code != 0 {
        fail(
            10,
            dir,
            &format!("spawned binary exited with code {}, expected 0", exit_code),
        );
    }

    pass(
        10,
        dir,
        &format!("exec from {}: binary ran and exited 0", dir),
    );
}

// -----------------------------------------------------------------------
// Test 11: an mmap address outside the user half is rejected, not mapped
// -----------------------------------------------------------------------
fn test11(dir: &str) {
    // The kernel builds a VirtAddr and a VMA range straight out of these, and
    // every VMA it holds becomes a user-accessible mapping. Each case must come
    // back as a failed mmap; the failure being tested for is the kernel taking
    // the value at its word.
    let cases: [(&str, u64, u64); 5] = [
        // Non-canonical: x86_64 has no such address at all.
        ("non-canonical", 0x0000_9000_0000_0000, PAGE),
        // Canonical, but in the kernel half.
        ("kernel half", 0xffff_8000_0000_0000, PAGE),
        // The last user page, extended one page past the top of the user half.
        ("straddles the top", 0x0000_7fff_ffff_f000, 2 * PAGE),
        // start + length wraps to a small address.
        ("wrapping length", 0x0000_7000_0000_0000, u64::MAX - PAGE),
        // Kernel-chosen address, but a length no gap can hold.
        ("unsatisfiable length", 0, u64::MAX - PAGE),
    ];

    for (name, addr, length) in cases {
        if let Ok(p) = mmap(
            addr as *mut u8,
            length,
            PROT_READ | PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        ) {
            fail(
                11,
                dir,
                &format!(
                    "mmap({}, addr={:#x}, len={:#x}) returned {:p}, expected failure",
                    name,
                    addr,
                    length,
                    p.as_ptr()
                ),
            );
        }
    }

    pass(11, dir, "out-of-bounds mmap addresses all rejected");
}

/// Write one byte in a forked child and report whether the child survived it.
///
/// A write the mapping does not allow must kill the child, so the child exiting
/// 0 is the failure: it means the store landed. Nothing else can tell the two
/// apart from inside the process that performed it.
fn child_survives_write(ptr: *mut u8) -> bool {
    let pid = process::fork();
    if pid == Ok(0) {
        unsafe { core::ptr::write_volatile(ptr, 0x5a) };
        std::process::exit(0);
    }
    let Ok(pid) = pid else {
        return false;
    };
    process::waitpid(pid) == 0
}

// -----------------------------------------------------------------------
// Test 12: mprotect changes what a mapping allows, in both directions
// -----------------------------------------------------------------------
fn test12(dir: &str) {
    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    )
    .unwrap_or_else(|e| {
        fail(
            12,
            dir,
            &format!("mmap of one anonymous page failed: {e:?}"),
        )
    })
    .as_ptr();
    unsafe { core::ptr::write_volatile(ptr, 0x11) };

    if mprotect(ptr, PAGE, PROT_READ).is_err() {
        fail(12, dir, "mprotect to PROT_READ failed");
    }
    if unsafe { core::ptr::read_volatile(ptr) } != 0x11 {
        fail(12, dir, "read-only mapping lost its contents");
    }
    if child_survives_write(ptr) {
        fail(12, dir, "a write to a PROT_READ mapping was allowed");
    }

    // Back to writable. The page is shared with nothing now, but it carried
    // COW_BIT while the child above was alive, so this also covers restoring
    // write permission to a page that has been through a fork.
    if mprotect(ptr, PAGE, PROT_READ | PROT_WRITE).is_err() {
        fail(12, dir, "mprotect back to PROT_READ|PROT_WRITE failed");
    }
    unsafe { core::ptr::write_volatile(ptr, 0x22) };
    if unsafe { core::ptr::read_volatile(ptr) } != 0x22 {
        fail(12, dir, "write after restoring PROT_WRITE did not land");
    }

    let unmapped = (ptr as u64) + 0x1000_0000;
    let bad: [(&str, *mut u8, u64, u32); 4] = [
        (
            "unaligned addr",
            (ptr as u64 + 1) as *mut u8,
            PAGE,
            PROT_READ,
        ),
        ("zero length", ptr, 0, PROT_READ),
        ("unknown prot bit", ptr, PAGE, 0x40),
        ("unmapped range", unmapped as *mut u8, PAGE, PROT_READ),
    ];
    for (name, addr, len, prot) in bad {
        if mprotect(addr, len, prot).is_ok() {
            fail(
                12,
                dir,
                &format!("mprotect({name}) succeeded, expected a refusal"),
            );
        }
    }

    let _ = munmap(ptr, PAGE);
    pass(
        12,
        dir,
        "mprotect withdraws and restores write, and rejects bad ranges",
    );
}

// -----------------------------------------------------------------------
// Test 13: fork does not turn a read-only mapping into a writable one
// -----------------------------------------------------------------------
fn test13(dir: &str) {
    // The fork walk marks every anonymous page COW regardless of what its VMA
    // allows, so without a protection check on the COW path the child's write
    // is resolved by copying the frame and the store lands on a mapping that
    // was never writable. No mprotect is involved: PROT_READ from mmap is
    // enough to reach it.
    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ,
        MAP_PRIVATE | MAP_ANONYMOUS,
        -1,
        0,
    )
    .unwrap_or_else(|e| {
        fail(
            13,
            dir,
            &format!("mmap of one PROT_READ page failed: {e:?}"),
        )
    })
    .as_ptr();
    // Fault it in read-only, so fork has a present PTE to mark.
    if unsafe { core::ptr::read_volatile(ptr) } != 0 {
        fail(13, dir, "anonymous page was not zero-filled");
    }

    if child_survives_write(ptr) {
        fail(13, dir, "a forked child wrote to a PROT_READ mapping");
    }

    let _ = munmap(ptr, PAGE);
    pass(13, dir, "a COW page stays as unwritable as its VMA");
}

fn run_suite(dir: &str) {
    println!("mmaptest: running tests on [{}]", dir);
    let suite_start = Instant::now();

    let run = |n: u32, f: &dyn Fn(&str)| {
        let start = Instant::now();
        f(dir);
        let us = start.elapsed().as_micros();
        if us >= 10_000 {
            println!("  [{}] test {} finished in {} ms", dir, n, us / 1000);
        } else {
            println!("  [{}] test {} finished in {} us", dir, n, us);
        }
    };

    run(1, &test1);
    run(2, &test2);
    run(3, &test3);
    run(4, &test4);
    run(5, &test5);
    run(6, &test6);
    run(7, &test7);
    run(8, &test8);
    run(9, &test9);
    run(10, &test10);
    run(11, &test11);
    run(12, &test12);
    run(13, &test13);

    let total_ms = suite_start.elapsed().as_millis();
    println!("mmaptest: all tests passed [{}] ({} ms)", dir, total_ms);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() > 1 {
        // Run only the directories given on the command line.
        for dir in &args[1..] {
            run_suite(dir);
        }
    } else {
        // Default: run on EFS (/var) and memfs (/tmp).
        // FAT32 is not auto-mounted; pass its mountpoint explicitly if desired.
        run_suite("/var");
        run_suite("/tmp");
    }

    std::process::exit(0);
}
