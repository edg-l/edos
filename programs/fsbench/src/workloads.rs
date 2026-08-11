//! The benchmarks themselves.
//!
//! Three groups, each answering a different question:
//!
//! * `file_write` / `file_read` — what a program actually gets out of the
//!   filesystem, across the idioms a program actually uses.
//! * `metadata` — the per-file costs that dominate anything touching many
//!   small files.
//! * `raw_read` — the same bytes through `/dev/sdX`, which skips the
//!   filesystem entirely. The gap between this and `file_read` is EFS
//!   overhead; the gap between this and the host is the driver's.
//!
//! Reads are only honest when they miss the cache. Sequential file reads in
//! the same run as the write that produced them are page-cache hits, so the
//! read group is meant to run in a separate boot (`fsbench read`) against
//! files an earlier `fsbench write` left behind. Raw device reads need no such
//! care: the span is chosen larger than the block page cache, so it evicts
//! itself as it goes.

use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use edos_lib::io::{pread, pwrite};
use edos_lib::mem::{MAP_SHARED, MS_SYNC, PROT_READ, PROT_WRITE, mmap, msync, munmap};
use edos_lib::sys::{SYS_SYNC, syscall0};

use crate::counters::gauge;
use crate::harness::{Budget, Report, Rng, Runner, human_bytes};

/// Buffer sizes swept by the sequential tests, in bytes. Spans the range where
/// per-call overhead stops mattering: 512 is a sector, 4 KiB is one filesystem
/// block, 1 MiB is past the 992 KiB an AHCI command can carry in one go.
pub const SWEEP: &[usize] = &[512, 4096, 65536, 1 << 20];

/// Files the write phase leaves behind for the read phase, keyed by the buffer
/// size that produced them. One file per size keeps the read phase honest: it
/// reads a file it did not just write.
fn seq_path(dir: &str, size: usize) -> String {
    format!("{dir}/fsbench.seq.{size}")
}

fn rand_path(dir: &str) -> String {
    format!("{dir}/fsbench.rand")
}

fn mmap_path(dir: &str) -> String {
    format!("{dir}/fsbench.mmap")
}

/// Byte at `pos` of the file written with buffer size `tag`.
///
/// Position-dependent and tag-dependent, so a block landing at the wrong
/// offset, a stale block from a previous run, and a block of zeros that was
/// never written all fail the check. A constant fill catches none of those.
pub fn byte_at(tag: u64, pos: u64) -> u8 {
    let x = pos.wrapping_mul(2654435761) ^ (tag.wrapping_add(1).wrapping_mul(40503));
    (x >> 13) as u8
}

fn pattern_buf(tag: u64, offset: u64, len: usize) -> Vec<u8> {
    (0..len as u64).map(|i| byte_at(tag, offset + i)).collect()
}

// -----------------------------------------------------------------------
// Sequential writes
// -----------------------------------------------------------------------

/// `write(2)` in `chunk`-sized calls into a freshly created file.
///
/// Fresh on purpose: this is the allocating path, which has to find blocks and
/// update the bitmap as it goes. [`overwrite_seq`] measures the same bytes
/// without that cost, and the difference is what allocation costs.
pub fn write_seq(dir: &str, chunk: usize, budget: Budget) -> Report {
    let name = format!("write {}", human_bytes(chunk as u64));
    let path = seq_path(dir, chunk);
    let mut run = Runner::new(&name, "seq, allocating", budget);

    let _ = fs::remove_file(&path);
    let mut file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("create {path}: {e}"));
            return run.finish();
        }
    };

    let mut offset = 0u64;
    while run.keep_going() {
        let buf = pattern_buf(chunk as u64, offset, chunk);
        if run.op(chunk as u64, || file.write_all(&buf)).is_none() {
            break;
        }
        offset += chunk as u64;
    }
    drop(file);
    run.finish()
}

/// The same bytes over a file that already has its blocks.
pub fn overwrite_seq(dir: &str, chunk: usize, budget: Budget) -> Report {
    let name = format!("overwrite {}", human_bytes(chunk as u64));
    let path = seq_path(dir, chunk);
    let mut run = Runner::new(&name, "seq, in place", budget);

    let mut file = match OpenOptions::new().write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e}"));
            return run.finish();
        }
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < chunk as u64 {
        run.fail(format!("{path} is only {len} bytes"));
        return run.finish();
    }

    let mut offset = 0u64;
    while run.keep_going() {
        if offset + chunk as u64 > len {
            offset = 0;
            if file.seek(SeekFrom::Start(0)).is_err() {
                run.fail("seek failed".to_string());
                break;
            }
        }
        let buf = pattern_buf(chunk as u64, offset, chunk);
        if run.op(chunk as u64, || file.write_all(&buf)).is_none() {
            break;
        }
        offset += chunk as u64;
    }
    run.finish()
}

/// 512-byte writes funnelled through a 64 KiB `BufWriter`.
///
/// Isolates how much of the small-write penalty a program can buy back in
/// userspace without changing its own write size. Compare against
/// `write 512B`: the same calls from the program's point of view.
pub fn write_buffered(dir: &str, budget: Budget) -> Report {
    const SMALL: usize = 512;
    const BUFFER: usize = 64 * 1024;
    let path = format!("{dir}/fsbench.bufwriter");
    let mut run = Runner::new("write 512B", "via 64KiB BufWriter", budget);

    let _ = fs::remove_file(&path);
    let file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("create {path}: {e}"));
            return run.finish();
        }
    };
    let mut out = BufWriter::with_capacity(BUFFER, file);

    let mut offset = 0u64;
    while run.keep_going() {
        let buf = pattern_buf(SMALL as u64, offset, SMALL);
        if run.op(SMALL as u64, || out.write_all(&buf)).is_none() {
            break;
        }
        offset += SMALL as u64;
    }
    // The final flush is part of the cost; charge it as an operation of zero
    // bytes so it lands in the elapsed time without inflating throughput.
    run.op(0, || out.flush());
    run.finish()
}

/// One `fs::write` of the whole file: the single-syscall idiom.
pub fn write_whole_file(dir: &str, bytes: u64, budget: Budget) -> Report {
    let path = format!("{dir}/fsbench.whole");
    let mut run = Runner::new(
        &format!("fs::write {}", human_bytes(bytes)),
        "whole file, 1 call",
        budget,
    );

    let buf = pattern_buf(1, 0, bytes as usize);
    while run.keep_going() {
        let _ = fs::remove_file(&path);
        if run.op(bytes, || fs::write(&path, &buf)).is_none() {
            break;
        }
    }
    run.finish()
}

/// `pwrite` at explicit offsets, never moving the descriptor's own offset.
pub fn write_positional(dir: &str, chunk: usize, budget: Budget) -> Report {
    let path = format!("{dir}/fsbench.pwrite");
    let mut run = Runner::new(
        &format!("pwrite {}", human_bytes(chunk as u64)),
        "seq, positional",
        budget,
    );

    let file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("create {path}: {e}"));
            return run.finish();
        }
    };
    let fd = file.as_raw_fd() as u64;

    let mut offset = 0u64;
    while run.keep_going() {
        let buf = pattern_buf(chunk as u64, offset, chunk);
        let done = run.op(chunk as u64, || match pwrite(fd, &buf, offset) {
            n if n == chunk as isize => Ok(()),
            n => Err(format!("pwrite returned {n}, expected {chunk}")),
        });
        if done.is_none() {
            break;
        }
        offset += chunk as u64;
    }
    run.finish()
}

/// `write(2)` followed by `fsync` on every call.
///
/// This is the only write number on the list that measures the disk. Every
/// other one measures the page cache: a plain `write` returns as soon as the
/// bytes are in memory, and the device does not see them until writeback runs,
/// long after the benchmark that produced them has stopped timing. Durability
/// is what an installer, a database or a log writer actually pays for.
pub fn write_durable(dir: &str, chunk: usize, budget: Budget) -> Report {
    let name = format!("write {}", human_bytes(chunk as u64));
    let path = format!("{dir}/fsbench.fsync");
    let mut run = Runner::new(&name, "+ fsync each", budget);

    let _ = fs::remove_file(&path);
    let mut file = match File::create(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("create {path}: {e}"));
            return run.finish();
        }
    };

    // Drain first, untimed. Every test before this one returned as soon as its
    // bytes were in the page cache, so without this the first `fsync` pays for
    // their backlog and the number measures which test happened to run first
    // rather than what a durable write costs.
    unsafe { syscall0(SYS_SYNC) };

    let mut offset = 0u64;
    while run.keep_going() {
        let buf = pattern_buf(chunk as u64, offset, chunk);
        let done = run.op(chunk as u64, || -> std::io::Result<()> {
            file.write_all(&buf)?;
            file.sync_all()
        });
        if done.is_none() {
            break;
        }
        offset += chunk as u64;
    }
    let _ = fs::remove_file(&path);
    run.finish()
}

/// 4 KiB writes at random block-aligned offsets inside an existing file.
///
/// The interesting number here is IOPS, not MiB/s: this is the shape that a
/// database or a filesystem's own metadata traffic produces.
pub fn write_random(dir: &str, file_bytes: u64, budget: Budget) -> Report {
    const BLOCK: usize = 4096;
    let path = rand_path(dir);
    let mut run = Runner::new("pwrite 4KiB", "random offsets", budget);

    if let Err(e) = ensure_file(&path, file_bytes) {
        run.fail(e);
        return run.finish();
    }
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e}"));
            return run.finish();
        }
    };
    let fd = file.as_raw_fd() as u64;
    let blocks = file_bytes / BLOCK as u64;
    let mut rng = Rng::new(0x5eed_4ea1);

    while run.keep_going() {
        let offset = rng.below(blocks) * BLOCK as u64;
        let buf = pattern_buf(BLOCK as u64, offset, BLOCK);
        let done = run.op(BLOCK as u64, || match pwrite(fd, &buf, offset) {
            n if n == BLOCK as isize => Ok(()),
            n => Err(format!("pwrite returned {n}")),
        });
        if done.is_none() {
            break;
        }
    }
    run.finish()
}

/// `MAP_SHARED` store loop plus `msync`, the mapped-file write idiom.
pub fn write_mmap(dir: &str, bytes: u64, budget: Budget) -> Report {
    let path = mmap_path(dir);
    let mut run = Runner::new(
        &format!("mmap store {}", human_bytes(bytes)),
        "+ msync",
        budget,
    );

    if let Err(e) = ensure_file(&path, bytes) {
        run.fail(e);
        return run.finish();
    }
    let file = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e}"));
            return run.finish();
        }
    };
    let ptr = mmap(
        core::ptr::null_mut(),
        bytes,
        PROT_READ | PROT_WRITE,
        MAP_SHARED,
        file.as_raw_fd(),
        0,
    );
    if ptr.is_null() || ptr as isize == -1 {
        run.fail("mmap failed".to_string());
        return run.finish();
    }

    while run.keep_going() {
        let done = run.op(bytes, || {
            let dst = unsafe { core::slice::from_raw_parts_mut(ptr, bytes as usize) };
            for (i, slot) in dst.iter_mut().enumerate() {
                *slot = byte_at(2, i as u64);
            }
            match unsafe { msync(ptr, bytes, MS_SYNC) } {
                0 => Ok(()),
                e => Err(format!("msync returned {e}")),
            }
        });
        if done.is_none() {
            break;
        }
    }
    munmap(ptr, bytes);
    run.finish()
}

// -----------------------------------------------------------------------
// Reads
// -----------------------------------------------------------------------

/// `read(2)` in `chunk`-sized calls over a whole file, repeatedly.
///
/// The first pass is cold only if nothing has touched the file since boot;
/// later passes hit the per-inode page cache. `warm` says which claim the
/// caller is making, and only labels the result — it changes nothing.
pub fn read_seq(dir: &str, chunk: usize, warm: bool, budget: Budget) -> Report {
    let name = format!("read {}", human_bytes(chunk as u64));
    let path = seq_path(dir, chunk);
    let mut run = Runner::new(&name, if warm { "seq, warm" } else { "seq" }, budget);

    let mut file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e} (run `fsbench write` first)"));
            return run.finish();
        }
    };
    if warm {
        // Pull the whole file in once, untimed, so the measured pass is a
        // page-cache hit by construction rather than by hope.
        let mut sink = Vec::new();
        let _ = file.read_to_end(&mut sink);
        let _ = file.seek(SeekFrom::Start(0));
    }

    let mut buf = vec![0u8; chunk];
    while run.keep_going() {
        let n = run.op(chunk as u64, || file.read(&mut buf));
        match n {
            Some(0) => {
                if file.seek(SeekFrom::Start(0)).is_err() {
                    run.fail("seek failed".to_string());
                    break;
                }
            }
            Some(_) => {}
            None => break,
        }
    }
    run.finish()
}

/// `pread` at explicit offsets.
pub fn read_positional(dir: &str, chunk: usize, budget: Budget) -> Report {
    let path = seq_path(dir, chunk);
    let mut run = Runner::new(
        &format!("pread {}", human_bytes(chunk as u64)),
        "seq, positional",
        budget,
    );

    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e}"));
            return run.finish();
        }
    };
    let fd = file.as_raw_fd() as u64;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < chunk as u64 {
        run.fail(format!("{path} is only {len} bytes"));
        return run.finish();
    }

    let mut buf = vec![0u8; chunk];
    let mut offset = 0u64;
    while run.keep_going() {
        if offset + chunk as u64 > len {
            offset = 0;
        }
        let done = run.op(chunk as u64, || match pread(fd, &mut buf, offset) {
            n if n == chunk as isize => Ok(()),
            n => Err(format!("pread returned {n}, expected {chunk}")),
        });
        if done.is_none() {
            break;
        }
        offset += chunk as u64;
    }
    run.finish()
}

/// 4 KiB reads at random offsets: the IOPS number.
pub fn read_random(dir: &str, budget: Budget) -> Report {
    const BLOCK: usize = 4096;
    let path = rand_path(dir);
    let mut run = Runner::new("pread 4KiB", "random offsets", budget);

    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e} (run `fsbench write` first)"));
            return run.finish();
        }
    };
    let fd = file.as_raw_fd() as u64;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < BLOCK as u64 {
        run.fail(format!("{path} is only {len} bytes"));
        return run.finish();
    }
    let blocks = len / BLOCK as u64;
    let mut rng = Rng::new(0x5eed_4ea1);

    let mut buf = vec![0u8; BLOCK];
    while run.keep_going() {
        let offset = rng.below(blocks) * BLOCK as u64;
        let done = run.op(BLOCK as u64, || match pread(fd, &mut buf, offset) {
            n if n == BLOCK as isize => Ok(()),
            n => Err(format!("pread returned {n}")),
        });
        if done.is_none() {
            break;
        }
    }
    run.finish()
}

/// One `fs::read` of the whole file.
pub fn read_whole_file(dir: &str, chunk: usize, budget: Budget) -> Report {
    let path = seq_path(dir, chunk);
    let mut run = Runner::new("fs::read", "whole file, 1 call", budget);

    let len = match fs::metadata(&path) {
        Ok(m) => m.len(),
        Err(e) => {
            run.fail(format!("stat {path}: {e}"));
            return run.finish();
        }
    };
    while run.keep_going() {
        if run.op(len, || fs::read(&path)).is_none() {
            break;
        }
    }
    run.finish()
}

/// Mapped-file read: fault the pages in and sum them.
pub fn read_mmap(dir: &str, bytes: u64, budget: Budget) -> Report {
    let path = mmap_path(dir);
    let mut run = Runner::new(
        &format!("mmap load {}", human_bytes(bytes)),
        "faulted in",
        budget,
    );

    let file = match File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {path}: {e} (run `fsbench write` first)"));
            return run.finish();
        }
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0).min(bytes);
    if len == 0 {
        run.fail(format!("{path} is empty"));
        return run.finish();
    }

    let fd = file.as_raw_fd();
    while run.keep_going() {
        // Map, touch and unmap inside one timed operation. Remapping every
        // pass is the point: an established mapping has no faults left to
        // take. Leaving the mmap and munmap outside the timing was worse than
        // imprecise — it reported a rate computed over the whole loop while
        // the latency column covered only the memory sweep, and the two
        // disagreed by a factor of sixty.
        let done = run.op(len, || -> Result<(), String> {
            let ptr = mmap(core::ptr::null_mut(), len, PROT_READ, MAP_SHARED, fd, 0);
            if ptr.is_null() || ptr as isize == -1 {
                return Err("mmap failed".to_string());
            }
            let src = unsafe { core::slice::from_raw_parts(ptr, len as usize) };
            let mut sum = 0u64;
            for &b in src {
                sum = sum.wrapping_add(b as u64);
            }
            // Keep the loop from being optimized away without printing.
            core::hint::black_box(sum);
            munmap(ptr, len);
            Ok(())
        });
        if done.is_none() {
            break;
        }
    }
    run.finish()
}

// -----------------------------------------------------------------------
// Metadata
// -----------------------------------------------------------------------

/// Create a one-byte file and unlink it, repeatedly.
pub fn meta_create_unlink(dir: &str, budget: Budget) -> Report {
    let mut run = Runner::new("create+unlink", "1-byte files", budget);
    let mut i = 0u64;
    while run.keep_going() {
        let path = format!("{dir}/fsbench.meta.{i}");
        let done = run.op(0, || -> Result<(), String> {
            let mut f = File::create(&path).map_err(|e| format!("create: {e}"))?;
            f.write_all(b"x").map_err(|e| format!("write: {e}"))?;
            drop(f);
            fs::remove_file(&path).map_err(|e| format!("unlink: {e}"))
        });
        if done.is_none() {
            break;
        }
        i += 1;
    }
    run.finish()
}

/// `stat` an existing file, repeatedly. Pure path lookup plus inode read.
pub fn meta_stat(dir: &str, budget: Budget) -> Report {
    let mut run = Runner::new("stat", "existing file", budget);
    let path = seq_path(dir, SWEEP[0]);
    if fs::metadata(&path).is_err() {
        run.fail(format!("{path} missing (run `fsbench write` first)"));
        return run.finish();
    }
    while run.keep_going() {
        if run.op(0, || fs::metadata(&path)).is_none() {
            break;
        }
    }
    run.finish()
}

/// List a directory, repeatedly. Counted in entries, not calls.
pub fn meta_readdir(dir: &str, budget: Budget) -> Report {
    let mut run = Runner::new("readdir", "entries/s", budget);
    let mut entries = 0u64;
    while run.keep_going() {
        let n = run.op(0, || -> Result<u64, String> {
            let iter = fs::read_dir(dir).map_err(|e| format!("read_dir: {e}"))?;
            Ok(iter.filter(|e| e.is_ok()).count() as u64)
        });
        match n {
            Some(n) => entries += n,
            None => break,
        }
    }
    let mut report = run.finish();
    // `ops` counted calls; the useful rate is entries per second.
    report.ops = entries;
    report
}

// -----------------------------------------------------------------------
// Raw block device
// -----------------------------------------------------------------------

/// Sequential reads straight from a block device, skipping the filesystem.
///
/// `span` is deliberately larger than the block page cache so the run keeps
/// missing it. Reads start at `skip` bytes, which lets a caller stay clear of
/// a live filesystem's hot metadata if they want to. Both are clamped to the
/// device, so a small device such as the boot ramdisk is swept end to end
/// rather than failing on the first read past its last sector.
pub fn raw_read(device: &str, chunk: usize, skip: u64, span: u64, budget: Budget) -> Report {
    let name = format!("read {}", human_bytes(chunk as u64));
    let mut run = Runner::new(&name, "raw device, seq", budget);

    let file = match File::open(device) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {device}: {e}"));
            return run.finish();
        }
    };
    let fd = file.as_raw_fd() as u64;

    let capacity = file.metadata().map(|m| m.len()).unwrap_or(0);
    if capacity < chunk as u64 {
        run.fail(format!("{device} holds {capacity} bytes"));
        return run.finish();
    }
    // A device too small to hold `skip` plus a useful span is swept from zero
    // instead. Clamping `skip` towards the end would leave a span of one
    // chunk, which is re-read from cache and measures nothing.
    let skip = if skip + chunk as u64 >= capacity {
        0
    } else {
        skip
    };
    let span = span.min(capacity - skip);

    let mut buf = vec![0u8; chunk];
    let mut offset = skip;
    let end = skip + span;
    while run.keep_going() {
        if offset + chunk as u64 > end {
            offset = skip;
        }
        let at = offset;
        let done = run.op(chunk as u64, || match pread(fd, &mut buf, at) {
            n if n == chunk as isize => Ok(()),
            n => Err(format!("pread at {at} returned {n}")),
        });
        if done.is_none() {
            break;
        }
        offset += chunk as u64;
    }
    run.finish()
}

/// Sequential writes straight to a block device, skipping the filesystem.
///
/// Destructive, which is why it is its own mode rather than part of a sweep.
/// The kernel refuses writes to a device with a mounted filesystem on it, so
/// the running system cannot be damaged by accident; an unmounted disk with
/// data on it can, and that is the caller's to know.
pub fn raw_write(device: &str, chunk: usize, skip: u64, span: u64, budget: Budget) -> Report {
    let name = format!("write {}", human_bytes(chunk as u64));
    let mut run = Runner::new(&name, "raw device, seq", budget);

    let file = match OpenOptions::new().write(true).open(device) {
        Ok(f) => f,
        Err(e) => {
            run.fail(format!("open {device} for writing: {e}"));
            return run.finish();
        }
    };
    let fd = file.as_raw_fd() as u64;

    let capacity = file.metadata().map(|m| m.len()).unwrap_or(0);
    if capacity < chunk as u64 {
        run.fail(format!("{device} holds {capacity} bytes"));
        return run.finish();
    }
    let skip = if skip + chunk as u64 >= capacity {
        0
    } else {
        skip
    };
    let span = span.min(capacity - skip);

    let mut offset = skip;
    let end = skip + span;
    while run.keep_going() {
        if offset + chunk as u64 > end {
            offset = skip;
        }
        let at = offset;
        let buf = pattern_buf(chunk as u64, at, chunk);
        let done = run.op(chunk as u64, || match pwrite(fd, &buf, at) {
            n if n == chunk as isize => Ok(()),
            n => Err(format!("pwrite at {at} returned {n}")),
        });
        if done.is_none() {
            break;
        }
        offset += chunk as u64;
    }
    run.finish()
}

// -----------------------------------------------------------------------
// Verification
// -----------------------------------------------------------------------

/// Re-read what the write phase produced and compare it against the pattern.
///
/// Returns one message per file that does not match. This is not timed: it is
/// the part of the run that catches a fast path that dropped data, which is
/// the failure mode every write-path bug in this tree has had.
pub fn verify(dir: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for &chunk in SWEEP {
        let path = seq_path(dir, chunk);
        let data = match fs::read(&path) {
            Ok(d) => d,
            Err(e) => {
                problems.push(format!("{path}: {e}"));
                continue;
            }
        };
        if data.is_empty() {
            problems.push(format!("{path}: empty"));
            continue;
        }
        // `overwrite_seq` rewrites a prefix of the same file with the same
        // pattern, so the whole file must still match position for position.
        let wrong = |i: usize| data[i] != byte_at(chunk as u64, i as u64);
        let Some(at) = (0..data.len()).find(|&i| wrong(i)) else {
            continue;
        };
        // What the damage looks like is what identifies it: a short run means
        // one bad write, a run to end-of-file means a lost tail, and zeros
        // mean the bytes were never stored at all.
        let bad = (at..data.len()).filter(|&i| wrong(i)).count();
        let tail_zero = data[at..].iter().all(|&b| b == 0);
        problems.push(format!(
            "{path}: {bad} of {} bytes wrong from offset {at} (block {}, {} from EOF); \
             got {:#04x} want {:#04x}{}",
            data.len(),
            at / 4096,
            data.len() - at,
            data[at],
            byte_at(chunk as u64, at as u64),
            if tail_zero { "; tail is all zeros" } else { "" }
        ));
    }
    problems
}

/// Delete everything the suite created.
pub fn cleanup(dir: &str) {
    for &chunk in SWEEP {
        let _ = fs::remove_file(seq_path(dir, chunk));
    }
    for name in [
        "fsbench.rand",
        "fsbench.mmap",
        "fsbench.whole",
        "fsbench.bufwriter",
        RA_NAME,
    ] {
        let _ = fs::remove_file(format!("{dir}/{name}"));
    }
    // Leftover metadata files from a run that was interrupted mid-loop.
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("fsbench.meta.") {
                let _ = fs::remove_file(format!("{dir}/{name}"));
            }
        }
    }
}

/// Create `path` with `bytes` of pattern data if it is not already that long.
fn ensure_file(path: &str, bytes: u64) -> Result<(), String> {
    if let Ok(meta) = fs::metadata(path)
        && meta.len() >= bytes
    {
        return Ok(());
    }
    let buf = pattern_buf(3, 0, bytes as usize);
    fs::write(path, &buf).map_err(|e| format!("prepare {path}: {e}"))
}

// -----------------------------------------------------------------------
// Readahead instrument
// -----------------------------------------------------------------------
//
// The rest of the suite cannot see readahead work. Its sequential files are
// small enough that the kernel's whole-file prefetch turns a first read into
// one bulk fill (`RA_WHOLE_FILE_MAX_PAGES`, 2 MiB), and every read after that
// is a page-cache hit. Above that threshold the window ramps instead, and the
// question is whether the prefetch runs ahead of the reader or trails it.
//
// That question is not answered by throughput alone, so this pass reports two
// things beside it: how many calls waited on I/O nobody had started, and how
// much was in flight at the device between calls. A build that claims to
// pipeline readahead and leaves `ncq_inflight` at 0 between every call has
// pipelined nothing.

const RA_NAME: &str = "fsbench.ra";

/// Pattern tag for the readahead file. Distinct from every `SWEEP` size and
/// from the tag [`ensure_file`] uses, so a block from another test that lands
/// here fails the edge check.
const RA_TAG: u64 = 7;

/// Call size for the pass. One AHCI command carries 992 KiB and the readahead
/// window is 512 KiB, so 64 KiB is small enough that several calls fall inside
/// one window and the ones served from it can be told from the ones that wait.
pub const RA_CHUNK: usize = 64 << 10;

/// A call this much slower than the median waited on the device rather than on
/// the page cache. The gap between the two is three orders of magnitude, so the
/// exact factor does not matter; the count does.
const RA_STALL_FACTOR: u64 = 4;

fn ra_path(dir: &str) -> String {
    format!("{dir}/{RA_NAME}")
}

/// Write the file [`ra_read`] reads, and get it onto the disk.
///
/// Separate mode on purpose: the pass is only cold in a boot that has not
/// touched the file, so this runs, the machine reboots, and `fsbench ra` reads
/// what is left behind.
pub fn ra_prepare(dir: &str, bytes: u64) -> Result<(String, u64), String> {
    let path = ra_path(dir);
    let _ = fs::remove_file(&path);
    let mut file = File::create(&path).map_err(|e| format!("create {path}: {e}"))?;

    const STEP: u64 = 1 << 20;
    let mut offset = 0u64;
    while offset < bytes {
        let len = STEP.min(bytes - offset) as usize;
        let buf = pattern_buf(RA_TAG, offset, len);
        file.write_all(&buf)
            .map_err(|e| format!("write {path} at {offset}: {e}"))?;
        offset += len as u64;
    }
    file.sync_all().map_err(|e| format!("fsync {path}: {e}"))?;
    drop(file);
    // The file's own data is durable after the fsync; this drains the metadata
    // and the journal, so the reboot cannot lose the extents that name it.
    unsafe { syscall0(SYS_SYNC) };
    Ok((path, offset))
}

/// What one cold sequential pass over the large file showed.
pub struct RaReport {
    pub path: String,
    pub bytes: u64,
    pub calls: u64,
    /// Summed time inside `read`, which is the read path's own cost. The wall
    /// clock also carries the between-call sampling, so it is reported apart.
    pub read_time: Duration,
    pub wall: Duration,
    pub p50: u64,
    pub p99: u64,
    pub max: u64,
    /// Calls slower than `RA_STALL_FACTOR` x p50, and the bound that decided.
    pub stalls: u64,
    pub stall_bound: u64,
    /// `ncq_inflight` sampled once after every call.
    pub inflight_samples: u64,
    pub inflight_nonzero: u64,
    pub inflight_max: u64,
    /// `ncq_max_inflight` either side of the pass. It is a high-water mark that
    /// nothing resets, so the boot's own I/O is already in `before` and only
    /// the rise is this pass's.
    pub hwm_before: u64,
    pub hwm_after: u64,
    /// First edge check that did not match, if any.
    pub mismatch: Option<String>,
}

impl RaReport {
    pub fn mib_per_sec(&self) -> f64 {
        let secs = self.read_time.as_secs_f64();
        if secs <= 0.0 {
            return 0.0;
        }
        (self.bytes as f64 / (1024.0 * 1024.0)) / secs
    }
}

/// Read the whole readahead file front to back in [`RA_CHUNK`] calls.
///
/// Every call is timed, and `ncq_inflight` is sampled between calls rather than
/// inside them, so a sample cannot inflate a latency. Sampling does delay the
/// next call by a procfs read; that cost is identical in both arms of an A/B,
/// which is the property the comparison needs.
pub fn ra_read(dir: &str) -> Result<RaReport, String> {
    let path = ra_path(dir);
    let mut file =
        File::open(&path).map_err(|e| format!("open {path}: {e} (run `fsbench raprep` first)"))?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    if size < 4 << 20 {
        return Err(format!(
            "{path} is {} — too small to ride the ramping window, re-run `fsbench raprep`",
            human_bytes(size)
        ));
    }

    let hwm_before = gauge("/proc/ahci_stats", "ncq_max_inflight");
    let mut buf = vec![0u8; RA_CHUNK];
    let mut samples: Vec<u64> = Vec::new();
    let mut inflight_nonzero = 0u64;
    let mut inflight_max = 0u64;
    let mut read_time = Duration::ZERO;
    let mut mismatch = None;
    let mut offset = 0u64;

    let wall_start = Instant::now();
    loop {
        let t0 = Instant::now();
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {path} at {offset}: {e}"))?;
        let dt = t0.elapsed();
        if n == 0 {
            break;
        }
        read_time += dt;
        samples.push(dt.as_nanos() as u64);

        let inflight = gauge("/proc/ahci_stats", "ncq_inflight");
        inflight_max = inflight_max.max(inflight);
        if inflight > 0 {
            inflight_nonzero += 1;
        }
        if mismatch.is_none() {
            mismatch = ra_check_edges(&buf[..n], offset);
        }
        offset += n as u64;
    }
    let wall = wall_start.elapsed();
    let hwm_after = gauge("/proc/ahci_stats", "ncq_max_inflight");

    let calls = samples.len() as u64;
    let max = samples.iter().copied().max().unwrap_or(0);
    let mut sorted = samples.clone();
    sorted.sort_unstable();
    let pick = |q: f64| -> u64 {
        if sorted.is_empty() {
            return 0;
        }
        sorted[((sorted.len() as f64 - 1.0) * q).round() as usize]
    };
    let p50 = pick(0.50);
    let stall_bound = p50.saturating_mul(RA_STALL_FACTOR);
    let stalls = samples.iter().filter(|&&s| s > stall_bound).count() as u64;

    Ok(RaReport {
        path,
        bytes: offset,
        calls,
        read_time,
        wall,
        p50,
        p99: pick(0.99),
        max,
        stalls,
        stall_bound,
        inflight_samples: calls,
        inflight_nonzero,
        inflight_max,
        hwm_before,
        hwm_after,
        mismatch,
    })
}

/// Check the first and last 512 bytes of a chunk against the pattern.
///
/// Cheap on purpose: generating the pattern for every byte of a 16 MiB pass
/// costs more CPU than the reads it sits between, and delaying the reader is
/// exactly the thing the instrument is trying to observe. The edges still catch
/// a block landing at the wrong offset, a stale block from another test, and a
/// page of zeros that was never filled.
fn ra_check_edges(data: &[u8], offset: u64) -> Option<String> {
    const EDGE: usize = 512;
    let tail = data.len().saturating_sub(EDGE);
    for at in [0usize, tail] {
        let end = (at + EDGE).min(data.len());
        for i in at..end {
            let want = byte_at(RA_TAG, offset + i as u64);
            if data[i] != want {
                return Some(format!(
                    "byte {} of the file is {:#04x}, want {want:#04x}",
                    offset + i as u64,
                    data[i]
                ));
            }
        }
    }
    None
}
