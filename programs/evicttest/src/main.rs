// Deferred-eviction smoke test. Creates + unlinks N files in each directory
// given on the command line (default: /tmp /var), reads /proc/evict_stats
// before and after, and asserts the drain count advanced by at least N per
// directory.
//
// Exercises the `VfsInode::drop -> post_evict -> evict kthread -> evict_inode`
// pipeline. A regression that re-introduces blocking work in the drop path
// would either hang the orphan burst or fail the drain-count assertion.

use std::{
    fs::{self, File, read_to_string},
    io::Write,
    process::ExitCode,
};

const N_PER_DIR: u64 = 32;

fn read_drain_count() -> Option<u64> {
    let text = read_to_string("/proc/evict_stats").ok()?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("drain_count:") {
            return rest.trim().parse::<u64>().ok();
        }
    }
    None
}

fn wait_for_drain(target: u64, timeout_iters: u32) -> Option<u64> {
    for _ in 0..timeout_iters {
        if let Some(c) = read_drain_count() {
            if c >= target {
                return Some(c);
            }
        }
        // Busy-wait. The evict kthread runs concurrently; orphan processing
        // is fast (a few AHCI writes per inode for EFS, noop for memfs).
    }
    read_drain_count()
}

fn test_dir(dir: &str) -> Result<(), String> {
    let before =
        read_drain_count().ok_or_else(|| "failed to read /proc/evict_stats".to_string())?;
    println!("evicttest: {dir} drain_count before = {before}");

    for i in 0..N_PER_DIR {
        let path = format!("{dir}/.evicttest.{i}");
        {
            let mut f = File::create(&path).map_err(|e| format!("create {path}: {e}"))?;
            f.write_all(b"x")
                .map_err(|e| format!("write {path}: {e}"))?;
        }
        fs::remove_file(&path).map_err(|e| format!("unlink {path}: {e}"))?;
    }

    let target = before + N_PER_DIR;
    let after = wait_for_drain(target, 200_000)
        .ok_or_else(|| "failed to read /proc/evict_stats after".to_string())?;
    println!("evicttest: {dir} drain_count after  = {after} (expected >= {target})");

    if after < target {
        return Err(format!(
            "{dir}: drain_count advanced {} < expected {}",
            after - before,
            N_PER_DIR
        ));
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dirs: Vec<&str> = if args.is_empty() {
        vec!["/tmp", "/var"]
    } else {
        args.iter().map(|s| s.as_str()).collect()
    };

    let mut failed = false;
    for dir in dirs {
        match test_dir(dir) {
            Ok(()) => println!("evicttest: PASS {dir}"),
            Err(e) => {
                println!("evicttest: FAIL {dir}: {e}");
                failed = true;
            }
        }
    }

    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
