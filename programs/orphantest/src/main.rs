//! Hold unlinked-but-open files, so a power cut lands in the orphan window.
//!
//! An inode that has lost its last name but still has an open descriptor is on
//! EFS's orphan chain (`doc/efs.md` §14): allocated, named by nothing, and
//! pending deletion. That window is normally microseconds wide, which is why an
//! unclean shutdown inside it used to be something only a crash report showed.
//! This program holds it open on purpose and says so, so a test harness can cut
//! power while it is wide and check that the next mount finishes the deletions.
//!
//!     orphantest <dir> <count>
//!
//! Creates `<count>` files under `<dir>`, writes a block to each so they own
//! data blocks worth freeing, unlinks them while keeping every descriptor open,
//! reports through `/dev/klog`, and then waits. It never exits on its own: the
//! harness is expected to cut power, and the descriptors have to stay open until
//! it does.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::process::exit;

use edos_lib::io::Tee;

/// Guest output only reaches the host through `/dev/klog`; stdout goes to the
/// GUI terminal, which the serial capture never sees.
fn klog(line: &str) {
    Tee::new(true).line(line);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: orphantest <dir> <count>");
        exit(2);
    }
    let dir = args[1].trim_end_matches('/').to_string();
    let count: usize = match args[2].parse() {
        Ok(n) if n > 0 => n,
        _ => {
            eprintln!("count must be a positive integer");
            exit(2);
        }
    };

    // Held for the lifetime of the process: dropping one would let its inode be
    // evicted, which is the opposite of what this measures.
    let mut held: Vec<File> = Vec::with_capacity(count);

    for i in 0..count {
        let path = format!("{dir}/orphan_{i}");
        let mut f = match OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .read(true)
            .open(&path)
        {
            Ok(f) => f,
            Err(e) => {
                klog(&format!("ORPHANTEST_FAIL create {path}: {e}"));
                exit(1);
            }
        };
        if let Err(e) = f.write_all(&[0xABu8; 4096]) {
            klog(&format!("ORPHANTEST_FAIL write {path}: {e}"));
            exit(1);
        }
        if let Err(e) = std::fs::remove_file(&path) {
            klog(&format!("ORPHANTEST_FAIL unlink {path}: {e}"));
            exit(1);
        }
        held.push(f);
    }

    // Everything the files wrote is on its way to the disk before the cut; the
    // point is the inodes, not the data.
    for f in &held {
        let _ = f.sync_all();
    }

    klog(&format!("ORPHANTEST_HOLDING {count}"));

    // The descriptors must outlive this program's usefulness, so it waits rather
    // than returning. A harness cuts power here.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
