//! Durability check for file writes: does what a program wrote actually reach
//! the disk?
//!
//! Split into two phases on purpose. `write` fills a directory and syncs;
//! `verify` recomputes the same content and compares. They must run in
//! *different boots*, because a check in the same boot reads the page cache
//! and passes even when nothing was written. Every corruption this test
//! exists for was invisible warm and obvious cold.
//!
//! `scripts/fs-regression` drives both halves. The verdict also goes to
//! `/dev/klog` so the host can read it off the serial log.

use std::fs;
use std::io::Write;

/// File sizes, in bytes. Chosen to cover the shapes that broke:
/// sub-page, exactly one page, a partial tail, and enough pages to outrun
/// the journal ring and force a checkpoint mid-write.
const SIZES: &[usize] = &[100, 4096, 4096 * 3 + 7, 4096 * 64, 4096 * 700];

/// Byte at `pos` of file `n`. A per-file mixed sequence: a wrong block, a
/// stale block and a zeroed block all fail, and a block written at the wrong
/// offset fails too, which a constant fill would not catch.
fn byte_at(n: usize, pos: usize) -> u8 {
    let x = (pos as u64).wrapping_mul(2654435761) ^ ((n as u64 + 1).wrapping_mul(40503));
    (x >> 13) as u8
}

fn content(n: usize, len: usize) -> Vec<u8> {
    (0..len).map(|pos| byte_at(n, pos)).collect()
}

fn report(verdict: &str) {
    println!("{verdict}");
    if let Ok(mut klog) = fs::OpenOptions::new().write(true).open("/dev/klog") {
        let _ = writeln!(klog, "{verdict}");
    }
}

fn usage() -> ! {
    eprintln!("Usage: fstest <write|verify> <dir>");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        usage();
    }
    let dir = args[2].clone();

    match args[1].as_str() {
        "write" => {
            if let Err(e) = fs::create_dir(&dir) {
                if e.kind() != std::io::ErrorKind::AlreadyExists {
                    report(&format!("FSTEST FAIL: mkdir {dir}: {e}"));
                    std::process::exit(1);
                }
            }
            for (n, &len) in SIZES.iter().enumerate() {
                let path = format!("{dir}/f{n}");
                if let Err(e) = fs::write(&path, content(n, len)) {
                    report(&format!("FSTEST FAIL: write {path}: {e}"));
                    std::process::exit(1);
                }
            }
            unsafe { edos_sync() };
            report(&format!("FSTEST WROTE {} files", SIZES.len()));
        }
        "verify" => {
            let mut bad = 0;
            for (n, &len) in SIZES.iter().enumerate() {
                let path = format!("{dir}/f{n}");
                match fs::read(&path) {
                    Err(e) => {
                        report(&format!("FSTEST FAIL: read {path}: {e}"));
                        bad += 1;
                    }
                    Ok(got) => {
                        let want = content(n, len);
                        if got.len() != want.len() {
                            report(&format!(
                                "FSTEST FAIL: {path} is {} bytes, expected {}",
                                got.len(),
                                want.len()
                            ));
                            bad += 1;
                        } else if let Some(at) = (0..len).find(|&i| got[i] != want[i]) {
                            let zeros = got.iter().all(|&b| b == 0);
                            report(&format!(
                                "FSTEST FAIL: {path} differs at byte {at}{}",
                                if zeros {
                                    " (all zeros: never written)"
                                } else {
                                    ""
                                }
                            ));
                            bad += 1;
                        }
                    }
                }
            }
            if bad == 0 {
                report(&format!("FSTEST PASS {} files", SIZES.len()));
            } else {
                report(&format!("FSTEST FAILED {bad} of {}", SIZES.len()));
                std::process::exit(1);
            }
        }
        _ => usage(),
    }
}

unsafe fn edos_sync() {
    unsafe {
        core::arch::asm!("syscall", in("rax") 162u64, out("rcx") _, out("r11") _);
    }
}
