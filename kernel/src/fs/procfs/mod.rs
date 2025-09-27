//! Process filesystem exposing live kernel thread information.

use core::{fmt::Write, sync::atomic::Ordering};

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::{
    fs::{Error, File, FileAttrs, FileKind, FileSystem, path::Path},
    syscalls::Errno,
    thread::thread::{State, Thread, ThreadId, get_thread_info_by_id, list_threads},
};

pub struct Procfs;

impl Procfs {
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }

    fn collect_snapshots() -> Vec<ThreadSnapshot> {
        let mut snapshots: Vec<ThreadSnapshot> = list_threads()
            .into_iter()
            .map(|thread| ThreadSnapshot::from_thread(thread.as_ref()))
            .collect();
        snapshots.sort_by_key(|snap| snap.tid);
        snapshots
    }

    fn dir_entry(name: String) -> File {
        File {
            name,
            kind: FileKind::Directory,
            size: 0,
            attrs: FileAttrs {
                readonly: true,
                hidden: false,
                system: false,
                archive: false,
            },
            created: None,
            accessed: None,
            modified: None,
        }
    }

    fn file_entry(name: String, size: usize) -> File {
        File {
            name,
            kind: FileKind::File,
            size: size as u64,
            attrs: FileAttrs {
                readonly: true,
                hidden: false,
                system: false,
                archive: false,
            },
            created: None,
            accessed: None,
            modified: None,
        }
    }

    fn render_process_table(entries: &[ThreadSnapshot]) -> String {
        let mut table = String::from("PID   TYPE   STATE     PRIO CPU CPUms NAME\n");
        for entry in entries {
            let ty = if entry.is_kernel { "kernel" } else { "user" };
            let state = format!("{:?}", entry.state);
            let name = entry.display_name();
            let cpu_ms = entry.cpu_time_ns / 1_000_000;
            let _ = writeln!(
                table,
                "{:<5} {:<6} {:<9} {:<4} {:<3} {:>6} {}",
                entry.tid, ty, state, entry.priority, entry.cpu, cpu_ms, name
            );
        }
        table
    }

    fn resolve_path(path: &Path) -> Result<ProcNode, Error> {
        if path.is_root() {
            return Ok(ProcNode::Root);
        }

        let components = path.components();

        match components.len() {
            1 => match components[0].as_str() {
                "processes" => Ok(ProcNode::ProcessesFile),
                tid_component => parse_tid(tid_component)
                    .map(ProcNode::ProcessDir)
                    .ok_or(Error::FileNotFound),
            },
            2 => {
                let tid = parse_tid(&components[0]).ok_or(Error::FileNotFound)?;
                match components[1].as_str() {
                    "status" => Ok(ProcNode::ProcessStatus(tid)),
                    "cmdline" => Ok(ProcNode::ProcessCmdline(tid)),
                    _ => Err(Error::FileNotFound),
                }
            }
            _ => Err(Error::FileNotFound),
        }
    }

    fn find_snapshot<'a>(snapshots: &'a [ThreadSnapshot], tid: u64) -> Option<&'a ThreadSnapshot> {
        snapshots.iter().find(|snap| snap.tid == tid)
    }

    fn read_text(content: String, offset: usize, count: usize) -> Vec<u8> {
        let bytes = content.into_bytes();
        if offset >= bytes.len() {
            return Vec::new();
        }
        let end = (offset + count).min(bytes.len());
        bytes[offset..end].to_vec()
    }
}

impl FileSystem for Procfs {
    fn list_files(&mut self, path: &Path) -> Result<Vec<File>, Error> {
        let path = path.normalize();
        match Self::resolve_path(&path)? {
            ProcNode::Root => {
                let snapshots = Self::collect_snapshots();
                let mut files = Vec::with_capacity(snapshots.len() + 1);

                let summary = Self::render_process_table(&snapshots);
                files.push(Self::file_entry("processes".to_string(), summary.len()));

                for snapshot in snapshots {
                    files.push(Self::dir_entry(snapshot.tid.to_string()));
                }

                Ok(files)
            }
            ProcNode::ProcessDir(tid) => {
                let snapshots = Self::collect_snapshots();
                let Some(snapshot) = Self::find_snapshot(&snapshots, tid) else {
                    return Err(Error::FileNotFound);
                };

                let mut files = Vec::with_capacity(2);
                files.push(Self::file_entry(
                    "status".to_string(),
                    snapshot.status_text().len(),
                ));
                files.push(Self::file_entry(
                    "cmdline".to_string(),
                    snapshot.cmdline_text().len(),
                ));

                Ok(files)
            }
            ProcNode::ProcessesFile | ProcNode::ProcessStatus(_) | ProcNode::ProcessCmdline(_) => {
                Err(Error::NotADir)
            }
        }
    }

    fn read_bytes(&mut self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let path = path.normalize();
        match Self::resolve_path(&path)? {
            ProcNode::ProcessesFile => {
                let snapshots = Self::collect_snapshots();
                let content = Self::render_process_table(&snapshots);
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::ProcessStatus(tid) => {
                let snapshots = Self::collect_snapshots();
                let Some(snapshot) = Self::find_snapshot(&snapshots, tid) else {
                    return Err(Error::FileNotFound);
                };
                let content = snapshot.status_text();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::ProcessCmdline(tid) => {
                let snapshots = Self::collect_snapshots();
                let Some(snapshot) = Self::find_snapshot(&snapshots, tid) else {
                    return Err(Error::FileNotFound);
                };
                let content = snapshot.cmdline_text();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::Root | ProcNode::ProcessDir(_) => Err(Error::NotAFile),
        }
    }

    fn write_bytes(&mut self, _path: &Path, _offset: usize, _data: &[u8]) -> Result<u64, Error> {
        Err(Error::Unsupported)
    }

    fn create_file(&mut self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn create_dir(&mut self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn remove_dir(&mut self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn remove_file(&mut self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn file_info(&mut self, path: &Path) -> Result<File, Error> {
        let path = path.normalize();
        match Self::resolve_path(&path)? {
            ProcNode::Root => Ok(Self::dir_entry(String::new())),
            ProcNode::ProcessesFile => {
                let snapshots = Self::collect_snapshots();
                let summary = Self::render_process_table(&snapshots);
                Ok(Self::file_entry("processes".to_string(), summary.len()))
            }
            ProcNode::ProcessDir(tid) => {
                let snapshots = Self::collect_snapshots();
                if Self::find_snapshot(&snapshots, tid).is_none() {
                    Err(Error::FileNotFound)
                } else {
                    Ok(Self::dir_entry(path.filename()))
                }
            }
            ProcNode::ProcessStatus(tid) => {
                let snapshots = Self::collect_snapshots();
                let Some(snapshot) = Self::find_snapshot(&snapshots, tid) else {
                    return Err(Error::FileNotFound);
                };
                let status = snapshot.status_text();
                Ok(Self::file_entry("status".to_string(), status.len()))
            }
            ProcNode::ProcessCmdline(tid) => {
                let snapshots = Self::collect_snapshots();
                let Some(snapshot) = Self::find_snapshot(&snapshots, tid) else {
                    return Err(Error::FileNotFound);
                };
                let cmdline = snapshot.cmdline_text();
                Ok(Self::file_entry("cmdline".to_string(), cmdline.len()))
            }
        }
    }

    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct ThreadSnapshot {
    tid: u64,
    name: String,
    state: State,
    priority: u8,
    cpu: u32,
    cpu_affinity: u32,
    flags: u32,
    slice_deadline: u64,
    sleep_deadline: u64,
    cpu_time_ns: u64,
    exit_code: i32,
    kstack_top: u64,
    is_kernel: bool,
    user_pid: Option<u64>,
    user_id: Option<u32>,
    group_id: Option<u32>,
    cwd: Option<String>,
    heap_break: Option<u64>,
    memory_regions: Option<usize>,
    memory_mappings: Option<usize>,
    next_mmap_addr: Option<u64>,
    errno: Option<Errno>,
}

impl ThreadSnapshot {
    fn from_thread(thread: &Thread) -> Self {
        let tid = thread.id.0;
        let name_str = thread.name.as_str();
        let name = if name_str.is_empty() {
            format!("thread-{tid}")
        } else {
            name_str.to_string()
        };

        let state = thread.state();
        let priority = thread.priority();
        let cpu = thread.cpu.load(Ordering::Acquire);
        let cpu_affinity = thread.cpu_affinity.load(Ordering::Acquire);
        let flags = thread.flags.load(Ordering::Acquire);
        let slice_deadline = thread.slice_deadline.load(Ordering::Acquire);
        let sleep_deadline = thread.sleep_deadline.load(Ordering::Acquire);
        let cpu_time_ns = thread.cpu_time_ns();
        let exit_code = thread.exit_code.load(Ordering::Acquire);
        let kstack_top = thread.kstack_top;
        let is_kernel = thread.user.is_none();

        let (user_pid, heap_break, memory_regions) = thread
            .user
            .as_ref()
            .map(|user_arc| {
                let user = user_arc.read();
                (
                    Some(user.pid),
                    Some(user.heap_break),
                    Some(user.memory_regions.len()),
                )
            })
            .unwrap_or((None, None, None));

        let (user_id, group_id, cwd, errno, memory_mappings, next_mmap_addr) =
            read_thread_info(thread.id);

        Self {
            tid,
            name,
            state,
            priority,
            cpu,
            cpu_affinity,
            flags,
            slice_deadline,
            sleep_deadline,
            cpu_time_ns,
            exit_code,
            kstack_top,
            is_kernel,
            user_pid,
            user_id,
            group_id,
            cwd,
            heap_break,
            memory_regions,
            memory_mappings,
            next_mmap_addr,
            errno,
        }
    }

    fn display_name(&self) -> &str {
        self.name.as_str()
    }

    fn cmdline_text(&self) -> String {
        if self.is_kernel {
            format!("[kernel] {}\n", self.name)
        } else {
            format!("{}\n", self.name)
        }
    }

    fn status_text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Name: {}", self.name);
        let _ = writeln!(out, "ThreadId: {}", self.tid);
        let _ = writeln!(
            out,
            "Type: {}",
            if self.is_kernel { "kernel" } else { "user" }
        );
        let _ = writeln!(out, "State: {:?}", self.state);
        let _ = writeln!(out, "Priority: {}", self.priority);
        let _ = writeln!(out, "CPU: {}", self.cpu);
        let _ = writeln!(out, "CPU Affinity: 0x{:08x}", self.cpu_affinity);
        let cpu_time_ms = self.cpu_time_ns / 1_000_000;
        let cpu_time_frac = (self.cpu_time_ns / 1_000) % 1000;
        let _ = writeln!(
            out,
            "CPU Time: {}.{:03} ms ({} ns)",
            cpu_time_ms, cpu_time_frac, self.cpu_time_ns
        );
        let _ = writeln!(out, "Flags: 0x{:08x}", self.flags);
        let _ = writeln!(out, "Slice Deadline: {}", self.slice_deadline);
        let _ = writeln!(out, "Sleep Deadline: {}", self.sleep_deadline);
        let _ = writeln!(out, "Exit Code: {}", self.exit_code);
        let _ = writeln!(out, "Kernel Stack Top: 0x{:016x}", self.kstack_top);
        let _ = writeln!(out, "User PID: {}", display_option_decimal(self.user_pid));
        let _ = writeln!(
            out,
            "User ID: {}",
            display_option_decimal(self.user_id.map(u64::from))
        );
        let _ = writeln!(
            out,
            "Group ID: {}",
            display_option_decimal(self.group_id.map(u64::from))
        );
        let _ = writeln!(out, "CWD: {}", display_option_str(self.cwd.as_deref()));
        let _ = writeln!(out, "Heap Break: {}", display_option_hex(self.heap_break));
        let _ = writeln!(
            out,
            "Memory Regions: {}",
            display_option_decimal(self.memory_regions.map(|v| v as u64))
        );
        let _ = writeln!(
            out,
            "Memory Mappings: {}",
            display_option_decimal(self.memory_mappings.map(|v| v as u64))
        );
        let _ = writeln!(
            out,
            "Next mmap addr: {}",
            display_option_hex(self.next_mmap_addr)
        );
        let _ = writeln!(out, "Errno: {}", display_option_errno(self.errno));
        out
    }
}

fn parse_tid(component: &str) -> Option<u64> {
    component.parse().ok()
}

fn read_thread_info(
    tid: ThreadId,
) -> (
    Option<u32>,
    Option<u32>,
    Option<String>,
    Option<Errno>,
    Option<usize>,
    Option<u64>,
) {
    if let Some(info_arc) = get_thread_info_by_id(tid)
        && let Some(info) = info_arc.try_lock()
    {
        return (
            Some(info.user_id),
            Some(info.group_id),
            Some(format!("{}", info.cwd)),
            Some(info.errno),
            Some(info.memory_mappings.len()),
            Some(info.next_mmap_addr.as_u64()),
        );
    }
    (None, None, None, None, None, None)
}

fn display_option_decimal(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| v.to_string())
}

fn display_option_hex(value: Option<u64>) -> String {
    value.map_or_else(|| "-".to_string(), |v| format!("0x{:016x}", v))
}

fn display_option_str(value: Option<&str>) -> String {
    value.map_or_else(|| "-".to_string(), |v| v.to_string())
}

fn display_option_errno(value: Option<Errno>) -> String {
    value
        .map(|errno| format!("{:?}", errno))
        .unwrap_or_else(|| "-".to_string())
}

#[derive(Debug, Clone, Copy)]
enum ProcNode {
    Root,
    ProcessesFile,
    ProcessDir(u64),
    ProcessStatus(u64),
    ProcessCmdline(u64),
}
