//! Reading the kernel's thread table out of procfs.
//!
//! Nothing here follows the Linux layout. `/proc/processes` is a fixed-width
//! table with a trailer line, `/proc/<tid>/status` is one `Key: value` per
//! line, and `/proc/<tid>/cmdline` is a single line rather than a NUL-separated
//! vector. All three are parsed against what `kernel/src/fs/procfs` renders.

use std::fs;
use std::io;

/// One thread, as `/proc/processes` reports it.
pub struct Process {
    pub pid: u64,
    pub ppid: u64,
    /// Process group, which is what the shell puts a job in and what a
    /// terminal signal is delivered to.
    pub pgid: u64,
    /// `user` or `kernel`.
    pub kind: String,
    pub state: String,
    /// Scheduling priority.
    pub priority: String,
    /// The CPU the thread last ran on.
    pub cpu: u32,
    pub cpu_ms: u64,
    /// Memory faulted into this thread's address space, or `None` for a kernel
    /// thread, which has no address space of its own.
    pub rss_kib: Option<u64>,
    pub name: String,
}

impl Process {
    pub fn is_kernel(&self) -> bool {
        self.kind == "kernel"
    }

    pub fn is_running(&self) -> bool {
        self.state == "Running"
    }
}

/// The whole table, including what its trailer says about the system.
pub struct Table {
    pub processes: Vec<Process>,
    /// Statuses of exited threads nobody has collected. A number that only
    /// grows means something is leaking exit records.
    pub pending_exits: u64,
    pub init_pid: u64,
}

/// System memory, from `/proc/meminfo`.
pub struct Memory {
    pub total_kib: u64,
    pub used_kib: u64,
}

/// What `/proc/<tid>/{status,cmdline}` add for one thread. Every field the
/// kernel has no answer for is rendered as `-` there, so they stay strings.
pub struct Details {
    pub cmdline: String,
    pub priority: String,
    pub affinity: String,
    pub vmas: String,
    /// Address space reserved, against the `RSS` column's faulted-in part.
    pub vm_size: String,
    pub cwd: String,
}

/// Read and parse the process table, sorted by pid so a row keeps its place
/// across refreshes.
pub fn read_table() -> io::Result<Table> {
    let text = fs::read_to_string("/proc/processes")?;
    let mut table = Table {
        processes: Vec::new(),
        pending_exits: 0,
        init_pid: 0,
    };

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("pending exit statuses:") {
            let (pending, init) = rest.split_once("init pid:").unwrap_or((rest, ""));
            table.pending_exits = first_number(pending);
            table.init_pid = first_number(init);
        } else if let Some(process) = parse_row(line) {
            table.processes.push(process);
        }
    }

    table.processes.sort_by_key(|p| p.pid);
    Ok(table)
}

/// Parse one table row. The header and the blank line before the trailer fail
/// the pid parse and are dropped by the same check.
///
/// Every column the kernel prints is read, in its order, even the ones no
/// caller wants: skipping one by position is how a reader silently ends up a
/// column behind the day a new one is added in the middle.
fn parse_row(line: &str) -> Option<Process> {
    let mut fields = line.split_ascii_whitespace();
    let pid = fields.next()?.parse().ok()?;
    let ppid = fields.next()?.parse().ok()?;
    let pgid = fields.next()?.parse().ok()?;
    let kind = fields.next()?.to_string();
    let state = fields.next()?.to_string();
    let priority = fields.next()?.to_string();
    let cpu = fields.next()?.parse().ok()?;
    let cpu_ms = fields.next()?.parse().ok()?;
    // `-` where the kernel has no address space to measure.
    let rss_kib = fields.next()?.parse().ok();
    let name = fields.collect::<Vec<&str>>().join(" ");

    Some(Process {
        pid,
        ppid,
        pgid,
        kind,
        state,
        priority,
        cpu,
        cpu_ms,
        rss_kib,
        name,
    })
}

/// The first decimal number in `text`, or zero.
fn first_number(text: &str) -> u64 {
    text.split_ascii_whitespace()
        .find_map(|word| word.parse().ok())
        .unwrap_or(0)
}

/// Read system memory. `MemUsed` is rendered by the kernel; deriving it from
/// the other two is the fallback for a kernel that stops printing it.
pub fn read_memory() -> io::Result<Memory> {
    let text = fs::read_to_string("/proc/meminfo")?;
    let total_kib = field(&text, "MemTotal")
        .and_then(first_word_number)
        .unwrap_or(0);
    let free_kib = field(&text, "MemFree")
        .and_then(first_word_number)
        .unwrap_or(0);
    let used_kib = field(&text, "MemUsed")
        .and_then(first_word_number)
        .unwrap_or_else(|| total_kib.saturating_sub(free_kib));
    Ok(Memory {
        total_kib,
        used_kib,
    })
}

/// Read the detail a table row has no column for.
pub fn read_details(pid: u64) -> io::Result<Details> {
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let cmdline = fs::read_to_string(format!("/proc/{pid}/cmdline"))?;
    Ok(Details {
        cmdline: cmdline.trim().to_string(),
        priority: field(&status, "Priority").unwrap_or("-").to_string(),
        affinity: field(&status, "CPU Affinity").unwrap_or("-").to_string(),
        vmas: field(&status, "VMAs").unwrap_or("-").to_string(),
        vm_size: field(&status, "VM Size").unwrap_or("-").to_string(),
        cwd: field(&status, "CWD").unwrap_or("-").to_string(),
    })
}

/// The value of `key` in a `Key: value` block.
///
/// The key must be followed by the colon, so `CPU` does not answer with the
/// value of `CPU Time`.
fn field<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        line.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix(':'))
            .map(str::trim)
    })
}

fn first_word_number(value: &str) -> Option<u64> {
    value.split_ascii_whitespace().next()?.parse().ok()
}
