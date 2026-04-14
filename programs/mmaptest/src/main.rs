use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;

use edos_lib::mem::{MAP_PRIVATE, MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE, mmap, msync, munmap};
use edos_lib::process;

const PAGE: u64 = 4096;

fn fail(test: u32, dir: &str, msg: &str) -> ! {
    eprintln!("FAIL test {} [{}]: {}", test, dir, msg);
    std::process::exit(1);
}

fn pass(test: u32, dir: &str, detail: &str) {
    println!("PASS test {} [{}]: {}", test, dir, detail);
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

    let ptr = mmap(core::ptr::null_mut(), PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(1, dir, "mmap returned null/MAP_FAILED");
    }

    let mapped: Vec<u8> = unsafe { core::slice::from_raw_parts(ptr, PAGE as usize).to_vec() };
    munmap(ptr, PAGE);

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
    );
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(2, dir, "mmap returned null/MAP_FAILED");
    }

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

    munmap(ptr, PAGE);

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
    );
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(3, dir, "mmap returned null/MAP_FAILED");
    }

    unsafe { ptr.write(b'C') };

    let ret = unsafe { msync(ptr, PAGE, MS_SYNC) };
    if ret != 0 {
        fail(3, dir, &format!("msync returned {}", ret));
    }

    munmap(ptr, PAGE);

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
    );
    let ptr_b = mmap(
        core::ptr::null_mut(),
        PAGE,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd_b,
        0,
    );

    if ptr_a.is_null() || ptr_a as usize == usize::MAX {
        fail(4, dir, "mmap A returned null/MAP_FAILED");
    }
    if ptr_b.is_null() || ptr_b as usize == usize::MAX {
        fail(4, dir, "mmap B returned null/MAP_FAILED");
    }

    // Write 'D' via mapping A
    unsafe { ptr_a.write(b'D') };

    // Read via mapping B -- must see 'D' (same page cache frame)
    let seen = unsafe { ptr_b.read() };

    munmap(ptr_a, PAGE);
    munmap(ptr_b, PAGE);

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
    );
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(5, dir, "mmap returned null/MAP_FAILED");
    }

    // First page (in-file) must be readable
    let first = unsafe { ptr.read() };
    if first != b'X' {
        fail(5, dir, &format!("expected 'X' at byte 0, got {}", first));
    }

    // Fork a child; the child accesses the second page (past EOF), which must
    // trigger a kill (exit code 11 in EDOS).
    let child_pid = process::fork();
    if child_pid < 0 {
        munmap(ptr, PAGE * 2);
        fail(5, dir, "fork failed");
    }

    if child_pid == 0 {
        // Child: touch the past-EOF page -- kernel must kill us.
        // read_volatile prevents the compiler from optimizing out the load.
        let byte = unsafe { core::ptr::read_volatile(ptr.add(PAGE as usize)) };
        // Prints only if the access somehow didn't fault.
        println!("test5 child: unexpected byte {} past EOF", byte);
        std::process::exit(0);
    }

    // Parent: wait for the child
    let exit_code = process::waitpid(child_pid as u64);
    munmap(ptr, PAGE * 2);

    if exit_code == 11 {
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
    );
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(6, dir, "mmap returned null/MAP_FAILED");
    }

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
    let child_pid = process::fork();
    if child_pid < 0 {
        munmap(ptr, PAGE * 2);
        fail(6, dir, "fork failed");
    }

    if child_pid == 0 {
        // Child: access byte 4096 (past truncated end) -- should be killed.
        // read_volatile prevents the compiler from optimizing out the load.
        let byte = unsafe { core::ptr::read_volatile(ptr.add(PAGE as usize)) };
        println!("test6 child: unexpected byte {} after truncate", byte);
        std::process::exit(0);
    }

    let exit_code = process::waitpid(child_pid as u64);
    munmap(ptr, PAGE * 2);

    if exit_code == 11 {
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
    );
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(7, dir, "mmap returned null/MAP_FAILED");
    }

    unsafe { ptr.write(b'Z') };

    let ret = unsafe { msync(ptr, PAGE, MS_SYNC) };
    if ret != 0 {
        fail(7, dir, &format!("msync returned {}", ret));
    }

    munmap(ptr, PAGE);

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
    );
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(8, dir, "mmap returned null/MAP_FAILED");
    }

    let child_pid = process::fork();
    if child_pid < 0 {
        munmap(ptr, PAGE);
        fail(8, dir, "fork failed");
    }

    if child_pid == 0 {
        // Child: write to its private mapping. COW should isolate the
        // parent's view and not touch the on-disk file.
        unsafe { core::ptr::write_volatile(ptr, b'X') };
        let mine = unsafe { core::ptr::read_volatile(ptr) };
        if mine != b'X' {
            println!("test8 child: child saw {} after its own write", mine);
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    let exit_code = process::waitpid(child_pid as u64);
    if exit_code != 0 {
        munmap(ptr, PAGE);
        fail(8, dir, &format!("child exited {}, expected 0", exit_code));
    }

    // Parent view: byte 0 must still be 'A' (COW isolation).
    let parent_byte = unsafe { core::ptr::read_volatile(ptr) };
    munmap(ptr, PAGE);
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

    let ptr = mmap(
        core::ptr::null_mut(),
        PAGE * 2,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        fd,
        0,
    );

    // If this filesystem does not support page-cache mmap, mmap returns MAP_FAILED.
    // Skip gracefully rather than failing.
    if ptr.is_null() || ptr as usize == usize::MAX {
        drop(file);
        // Clean up the file we created; ignore errors (file may be gone).
        let _ = fs::remove_file(&path);
        println!("SKIP test 9 [{}]: mmap not supported on this fs", dir);
        return;
    }

    // Write 'V' at offset 0 through the mapping while the file is still linked.
    unsafe { core::ptr::write_volatile(ptr, b'V') };

    // Unlink the file. The kernel must keep the data alive because the mapping
    // pins the inode.
    fs::remove_file(&path).unwrap_or_else(|e| fail(9, dir, &format!("remove_file: {}", e)));

    // The mapping must still read 'V' after unlink (inode is pinned).
    let still_v = unsafe { core::ptr::read_volatile(ptr) };
    if still_v != b'V' {
        munmap(ptr, PAGE * 2);
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
    let ret = unsafe { msync(ptr, PAGE * 2, MS_SYNC) };
    if ret != 0 {
        munmap(ptr, PAGE * 2);
        fail(9, dir, &format!("msync after unlink returned {}", ret));
    }

    munmap(ptr, PAGE * 2);
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
    fs::copy("/bin/echo", &dst)
        .unwrap_or_else(|e| fail(10, dir, &format!("copy /bin/echo -> {}: {}", dst, e)));

    // Spawn the copy and wait for it to exit cleanly.
    let pid = process::spawn(&dst, &["hello from test10"], 0, 1, 2);
    if pid == u64::MAX {
        let _ = fs::remove_file(&dst);
        fail(10, dir, &format!("spawn {} returned MAX (not found?)", dst));
    }

    let exit_code = process::waitpid(pid);

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

fn run_suite(dir: &str) {
    println!("mmaptest: running tests on [{}]", dir);
    test1(dir);
    test2(dir);
    test3(dir);
    test4(dir);
    test5(dir);
    test6(dir);
    test7(dir);
    test8(dir);
    test9(dir);
    test10(dir);
    println!("mmaptest: all tests passed [{}]", dir);
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
