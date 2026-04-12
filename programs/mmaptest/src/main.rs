use std::fs::{self, File, OpenOptions};
use std::os::fd::AsRawFd;

use edos_lib::mem::{MAP_PRIVATE, MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE, mmap, msync, munmap};
use edos_lib::process;

const PAGE: u64 = 4096;

fn fail(test: u32, msg: &str) -> ! {
    eprintln!("FAIL test {}: {}", test, msg);
    std::process::exit(1);
}

fn pass(test: u32, detail: &str) {
    println!("PASS test {}: {}", test, detail);
}

// -----------------------------------------------------------------------
// Test 1: MAP_PRIVATE read -- verify mmap bytes match fs::read bytes
// -----------------------------------------------------------------------
fn test1() {
    let path = "/var/mmaptest_t1.dat";
    // Full-page content. Kernel requires page-aligned mmap length for
    // file-backed mappings.
    let mut content = vec![0u8; PAGE as usize];
    let hello = b"Hello, mmap world!  ";
    content[..hello.len()].copy_from_slice(hello);
    fs::write(path, &content).unwrap_or_else(|e| fail(1, &format!("write file: {}", e)));

    let expected: Vec<u8> = fs::read(path).unwrap_or_else(|e| fail(1, &format!("fs::read: {}", e)));

    let file = File::open(path).unwrap_or_else(|e| fail(1, &format!("open: {}", e)));
    let fd = file.as_raw_fd();

    let ptr = mmap(core::ptr::null_mut(), PAGE, PROT_READ, MAP_PRIVATE, fd, 0);
    if ptr.is_null() || ptr as usize == usize::MAX {
        fail(1, "mmap returned null/MAP_FAILED");
    }

    let mapped: Vec<u8> = unsafe { core::slice::from_raw_parts(ptr, PAGE as usize).to_vec() };
    munmap(ptr, PAGE);

    if mapped != expected {
        fail(
            1,
            &format!(
                "mmap first 20 {:?} != fs::read first 20 {:?}",
                &mapped[..20],
                &expected[..20]
            ),
        );
    }

    pass(
        1,
        &format!("first 20 bytes via mmap match fs::read: {:?}", &mapped[..20]),
    );
}

// -----------------------------------------------------------------------
// Test 2: MAP_PRIVATE COW write -- private write does NOT reach disk
// -----------------------------------------------------------------------
fn test2() {
    let path = "/var/mmaptest_t2.dat";
    let content = vec![b'A'; PAGE as usize];
    fs::write(path, &content).unwrap_or_else(|e| fail(2, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .unwrap_or_else(|e| fail(2, &format!("open: {}", e)));
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
        fail(2, "mmap returned null/MAP_FAILED");
    }

    // Write 'B' via private mapping
    unsafe { ptr.write(b'B') };
    let mapped_byte = unsafe { ptr.read() };
    if mapped_byte != b'B' {
        fail(2, &format!("expected 'B' in mapping, got {}", mapped_byte));
    }

    munmap(ptr, PAGE);

    // File on disk must still start with 'A'
    let disk_byte = fs::read(path).unwrap_or_else(|e| fail(2, &format!("re-read: {}", e)))[0];
    if disk_byte != b'A' {
        fail(
            2,
            &format!(
                "COW failed: disk byte should be 'A' but got '{}'",
                disk_byte as char
            ),
        );
    }

    pass(2, "COW write visible in mapping ('B'), disk still 'A'");
}

// -----------------------------------------------------------------------
// Test 3: MAP_SHARED write + msync -- write must reach disk after msync
// -----------------------------------------------------------------------
fn test3() {
    let path = "/var/mmaptest_t3.dat";
    let content = vec![b'A'; PAGE as usize];
    fs::write(path, &content).unwrap_or_else(|e| fail(3, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| fail(3, &format!("open: {}", e)));
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
        fail(3, "mmap returned null/MAP_FAILED");
    }

    unsafe { ptr.write(b'C') };

    let ret = unsafe { msync(ptr, PAGE, MS_SYNC) };
    if ret != 0 {
        fail(3, &format!("msync returned {}", ret));
    }

    munmap(ptr, PAGE);

    let disk_byte = fs::read(path).unwrap_or_else(|e| fail(3, &format!("re-read: {}", e)))[0];
    if disk_byte != b'C' {
        fail(
            3,
            &format!(
                "expected 'C' on disk after msync, got '{}'",
                disk_byte as char
            ),
        );
    }

    pass(3, "MAP_SHARED write + msync: disk byte is 'C'");
}

// -----------------------------------------------------------------------
// Test 4: MAP_SHARED two-mapper visibility within one process
// -----------------------------------------------------------------------
fn test4() {
    let path = "/var/mmaptest_t4.dat";
    let content = vec![b'A'; PAGE as usize];
    fs::write(path, &content).unwrap_or_else(|e| fail(4, &format!("write file: {}", e)));

    let file_a = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| fail(4, &format!("open A: {}", e)));
    let file_b = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| fail(4, &format!("open B: {}", e)));

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
        fail(4, "mmap A returned null/MAP_FAILED");
    }
    if ptr_b.is_null() || ptr_b as usize == usize::MAX {
        fail(4, "mmap B returned null/MAP_FAILED");
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
            &format!(
                "two-mapper visibility failed: expected 'D' via mapping B, got '{}'",
                seen as char
            ),
        );
    }

    pass(4, "write via mapping A visible via mapping B without msync");
}

// -----------------------------------------------------------------------
// Test 5: Past-EOF fault kills child (fork + deref second page of 4KB file)
// -----------------------------------------------------------------------
fn test5() {
    let path = "/var/mmaptest_t5.dat";
    let content = vec![b'X'; PAGE as usize]; // 4 KB
    fs::write(path, &content).unwrap_or_else(|e| fail(5, &format!("write file: {}", e)));

    let file = File::open(path).unwrap_or_else(|e| fail(5, &format!("open: {}", e)));
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
        fail(5, "mmap returned null/MAP_FAILED");
    }

    // First page (in-file) must be readable
    let first = unsafe { ptr.read() };
    if first != b'X' {
        fail(5, &format!("expected 'X' at byte 0, got {}", first));
    }

    // Fork a child; the child accesses the second page (past EOF), which must
    // trigger a kill (exit code 11 in EDOS).
    let child_pid = process::fork();
    if child_pid < 0 {
        munmap(ptr, PAGE * 2);
        fail(5, "fork failed");
    }

    if child_pid == 0 {
        // Child: touch the past-EOF page -- kernel must kill us
        let _byte = unsafe { ptr.add(PAGE as usize).read() };
        // Should never reach here
        std::process::exit(0);
    }

    // Parent: wait for the child
    let exit_code = process::waitpid(child_pid as u64);
    munmap(ptr, PAGE * 2);

    if exit_code == 11 {
        pass(5, "past-EOF fault killed child with code 11: ok");
    } else {
        fail(
            5,
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
fn test6() {
    let path = "/var/mmaptest_t6.dat";
    let content = vec![b'Y'; PAGE as usize * 2]; // 8 KB
    fs::write(path, &content).unwrap_or_else(|e| fail(6, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| fail(6, &format!("open: {}", e)));
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
        fail(6, "mmap returned null/MAP_FAILED");
    }

    // Both pages accessible before truncation
    let b0 = unsafe { ptr.read() };
    let b1 = unsafe { ptr.add(PAGE as usize).read() };
    if b0 != b'Y' || b1 != b'Y' {
        fail(6, "pre-truncate read failed");
    }

    // Truncate to 4 KB
    file.set_len(PAGE)
        .unwrap_or_else(|e| fail(6, &format!("set_len: {}", e)));

    // Fork a child to access the now-truncated second page
    let child_pid = process::fork();
    if child_pid < 0 {
        munmap(ptr, PAGE * 2);
        fail(6, "fork failed");
    }

    if child_pid == 0 {
        // Child: access byte 4096 (past truncated end) -- should be killed
        let _byte = unsafe { ptr.add(PAGE as usize).read() };
        std::process::exit(0);
    }

    let exit_code = process::waitpid(child_pid as u64);
    munmap(ptr, PAGE * 2);

    if exit_code == 11 {
        pass(6, "post-truncate fault killed child with code 11: ok");
    } else {
        fail(
            6,
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
fn test7() {
    let path = "/var/mmaptest_t7.dat";
    let content = vec![b'A'; PAGE as usize];
    fs::write(path, &content).unwrap_or_else(|e| fail(7, &format!("write file: {}", e)));

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap_or_else(|e| fail(7, &format!("open rw: {}", e)));
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
        fail(7, "mmap returned null/MAP_FAILED");
    }

    unsafe { ptr.write(b'Z') };

    let ret = unsafe { msync(ptr, PAGE, MS_SYNC) };
    if ret != 0 {
        fail(7, &format!("msync returned {}", ret));
    }

    munmap(ptr, PAGE);

    // fsync via std (file is still open)
    file.sync_all()
        .unwrap_or_else(|e| fail(7, &format!("sync_all: {}", e)));
    drop(file);

    // Re-open and read
    let disk_byte = fs::read(path).unwrap_or_else(|e| fail(7, &format!("re-read: {}", e)))[0];
    if disk_byte != b'Z' {
        fail(
            7,
            &format!(
                "expected 'Z' on disk after msync+fsync, got '{}'",
                disk_byte as char
            ),
        );
    }

    pass(7, "fsync + msync round-trip: byte 0 is 'Z'");
}

fn main() {
    println!("mmaptest: running 7 tests");

    test1();
    test2();
    test3();
    test4();
    test5();
    test6();
    test7();

    println!("mmaptest: all tests passed");
    std::process::exit(0);
}
