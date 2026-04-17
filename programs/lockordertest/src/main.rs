// Lock-order stats smoke test. Performs a representative workload
// (open / write / fsync / read / unlink on 10 files) then reads
// /proc/lock_order_stats and asserts that `inversions == 0`.
//
// In release builds the kernel counters stay at 0 regardless, so the test
// trivially passes. In debug builds a real inversion would panic the kernel
// before reaching this check; the counter provides a secondary signal if the
// panic path is somehow bypassed.

use std::{
    fs::{self, File},
    io::{Read, Write},
    process::ExitCode,
};

const N_FILES: usize = 10;
const WORKDIR: &str = "/tmp";

fn read_lock_order_stats() -> Option<(u64, usize)> {
    let mut text = String::new();
    File::open("/proc/lock_order_stats")
        .ok()?
        .read_to_string(&mut text)
        .ok()?;

    let mut inversions: Option<u64> = None;
    let mut max_depth: Option<usize> = None;

    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("inversions:") {
            inversions = rest.trim().parse().ok();
        } else if let Some(rest) = line.strip_prefix("max_depth:") {
            max_depth = rest.trim().parse().ok();
        }
    }

    Some((inversions?, max_depth?))
}

fn do_workload() -> Result<(), String> {
    for i in 0..N_FILES {
        let path = format!("{WORKDIR}/.lockordertest.{i}");

        // create + write
        {
            let mut f = File::create(&path).map_err(|e| format!("create {path}: {e}"))?;
            f.write_all(b"lock-order-test-data")
                .map_err(|e| format!("write {path}: {e}"))?;
            // fsync to exercise the journal / block-cache ranked paths
            f.sync_all().map_err(|e| format!("fsync {path}: {e}"))?;
        }

        // reopen + read back (exercises inode.pages, dirty_keys, and mm paths)
        {
            let mut buf = [0u8; 20];
            let mut f = File::open(&path).map_err(|e| format!("open {path}: {e}"))?;
            f.read_exact(&mut buf)
                .map_err(|e| format!("read {path}: {e}"))?;
        }

        // read via read_to_string (exercises VFS + page-cache ranked paths)
        {
            let mut content = String::new();
            File::open(&path)
                .map_err(|e| format!("open2 {path}: {e}"))?
                .read_to_string(&mut content)
                .map_err(|e| format!("read_to_string {path}: {e}"))?;
        }

        // unlink
        fs::remove_file(&path).map_err(|e| format!("unlink {path}: {e}"))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let before = match read_lock_order_stats() {
        Some(s) => s,
        None => {
            println!("lockordertest: FAIL: cannot read /proc/lock_order_stats");
            return ExitCode::from(1);
        }
    };
    println!(
        "lockordertest: before workload: inversions={} max_depth={}",
        before.0, before.1
    );

    if let Err(e) = do_workload() {
        println!("lockordertest: FAIL workload: {e}");
        return ExitCode::from(1);
    }

    let after = match read_lock_order_stats() {
        Some(s) => s,
        None => {
            println!("lockordertest: FAIL: cannot read /proc/lock_order_stats after workload");
            return ExitCode::from(1);
        }
    };
    println!(
        "lockordertest: after  workload: inversions={} max_depth={}",
        after.0, after.1
    );

    if after.0 != 0 {
        println!(
            "lockordertest: FAIL: {} lock-order inversion(s) detected",
            after.0
        );
        return ExitCode::from(1);
    }

    println!("lockordertest: PASS (inversions=0, max_depth={})", after.1);
    ExitCode::SUCCESS
}
