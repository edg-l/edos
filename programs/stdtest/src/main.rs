//! Checks the parts of `std` that answer from a syscall rather than from a stub.
//!
//! Each case here stands for something the platform layer used to decline or
//! answer wrongly, so a regression shows up as a named failure rather than as a
//! program that quietly does less than it claims.

use std::fs::{self, File};
use std::io::{ErrorKind, Read, Write};
use std::process::{Command, ExitCode, Stdio};
use std::thread;

fn main() -> ExitCode {
    let mut failed = 0;

    for (name, result) in [
        ("yield_now", check_yield()),
        ("available_parallelism", check_parallelism()),
        ("dev_null", check_dev_null()),
        ("dev_zero", check_dev_zero()),
        ("dev_full", check_dev_full()),
        ("command_output", check_command_output()),
        ("stdio_null", check_stdio_null()),
        ("child_kill", check_child_kill()),
        ("file_debug", check_file_debug()),
        ("permissions", check_permissions()),
    ] {
        match result {
            Ok(()) => println!("ok   {name}"),
            Err(why) => {
                println!("FAIL {name}: {why}");
                failed += 1;
            }
        }
    }

    if failed == 0 {
        println!("all passed");
        ExitCode::SUCCESS
    } else {
        println!("{failed} failed");
        ExitCode::from(1)
    }
}

/// A yield that does nothing is indistinguishable from one that works, so this
/// only proves it returns rather than trapping or hanging.
fn check_yield() -> Result<(), String> {
    for _ in 0..1000 {
        thread::yield_now();
    }
    Ok(())
}

fn check_parallelism() -> Result<(), String> {
    let count = thread::available_parallelism().map_err(|e| format!("{e}"))?;
    let online = fs::read_to_string("/proc/cpuinfo")
        .map_err(|e| format!("/proc/cpuinfo: {e}"))?
        .lines()
        .find_map(|line| line.strip_prefix("cpus online:"))
        .and_then(|v| v.trim().parse::<usize>().ok())
        .ok_or("no 'cpus online' line")?;

    if count.get() == online {
        Ok(())
    } else {
        Err(format!("reported {count}, /proc/cpuinfo says {online}"))
    }
}

fn check_dev_null() -> Result<(), String> {
    let mut sink = File::create("/dev/null").map_err(|e| format!("open: {e}"))?;
    sink.write_all(b"discarded")
        .map_err(|e| format!("write: {e}"))?;

    let mut source = File::open("/dev/null").map_err(|e| format!("open for read: {e}"))?;
    let mut buf = [0u8; 16];
    match source.read(&mut buf) {
        Ok(0) => Ok(()),
        Ok(n) => Err(format!("read returned {n} bytes, expected end of file")),
        Err(e) => Err(format!("read: {e}")),
    }
}

fn check_dev_zero() -> Result<(), String> {
    let mut source = File::open("/dev/zero").map_err(|e| format!("open: {e}"))?;
    let mut buf = [0xAAu8; 32];
    let n = source.read(&mut buf).map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Err("read returned end of file".into());
    }
    if buf[..n].iter().all(|&b| b == 0) {
        Ok(())
    } else {
        Err("read did not return zeroes".into())
    }
}

fn check_dev_full() -> Result<(), String> {
    let mut sink = File::create("/dev/full").map_err(|e| format!("open: {e}"))?;
    match sink.write_all(b"no room") {
        Ok(()) => Err("write succeeded, expected it to fail".into()),
        Err(e) if e.kind() == ErrorKind::StorageFull => Ok(()),
        Err(e) => Err(format!("failed with {:?}, expected StorageFull", e.kind())),
    }
}

/// The one that hangs rather than fails if the parent keeps the child's end of
/// the pipe: the read waits for an end of file that never arrives.
fn check_command_output() -> Result<(), String> {
    let out = Command::new("/bin/echo")
        .arg("captured")
        .output()
        .map_err(|e| format!("output: {e}"))?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.trim() != "captured" {
        return Err(format!("stdout was {stdout:?}, expected \"captured\""));
    }
    if !out.status.success() {
        return Err(format!("exit status {:?}", out.status));
    }
    Ok(())
}

fn check_stdio_null() -> Result<(), String> {
    let status = Command::new("/bin/echo")
        .arg("into the void")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("status: {e}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("exit status {status:?}"))
    }
}

fn check_child_kill() -> Result<(), String> {
    let mut child = Command::new("/bin/sleep")
        .arg("30")
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn: {e}"))?;

    child.kill().map_err(|e| format!("kill: {e}"))?;
    let status = child.wait().map_err(|e| format!("wait: {e}"))?;
    if status.success() {
        Err("killed child reported success".into())
    } else {
        Ok(())
    }
}

/// Formatting a `File` used to panic on a `todo!()`.
fn check_file_debug() -> Result<(), String> {
    let file = File::open("/proc/cpuinfo").map_err(|e| format!("open: {e}"))?;
    let rendered = format!("{file:?}");
    if rendered.contains("File") {
        Ok(())
    } else {
        Err(format!("rendered as {rendered:?}"))
    }
}

/// `readonly()` answered a hardcoded false, and `set_permissions` claimed to
/// have written a bit nothing stores.
fn check_permissions() -> Result<(), String> {
    let path = "/tmp/stdtest_perm";
    fs::write(path, b"x").map_err(|e| format!("write: {e}"))?;

    let meta = fs::metadata(path).map_err(|e| format!("metadata: {e}"))?;
    let mut perm = meta.permissions();
    if perm.readonly() {
        let _ = fs::remove_file(path);
        return Err("a file just created reports read-only".into());
    }

    perm.set_readonly(true);
    let reported = match fs::set_permissions(path, perm) {
        Ok(()) => Err("set_permissions succeeded, but nothing stores the bit".into()),
        Err(e) if e.kind() == ErrorKind::Unsupported => Ok(()),
        Err(e) => Err(format!("failed with {:?}, expected Unsupported", e.kind())),
    };

    let _ = fs::remove_file(path);
    reported
}
