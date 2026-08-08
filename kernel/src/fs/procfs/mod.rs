//! Process filesystem exposing live kernel thread information.

use core::{fmt::Write, sync::atomic::Ordering};

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use crate::{
    debug::lock_order::RANK_VMAS,
    fs::{
        Error, File, FileAttrs, FileKind, FileSystem, block_page_cache::BlockPageCache, path::Path,
    },
    memory::frame_allocator::frame_allocator,
    ranked_lock,
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
        let mut table = String::from("PID   PPID  TYPE   STATE     PRIO CPU CPUms NAME\n");
        for entry in entries {
            let ty = if entry.is_kernel { "kernel" } else { "user" };
            let state = format!("{:?}", entry.state);
            let name = entry.display_name();
            let cpu_ms = entry.cpu_time_ns / 1_000_000;
            let _ = writeln!(
                table,
                "{:<5} {:<5} {:<6} {:<9} {:<4} {:<3} {:>6} {}",
                entry.tid, entry.parent, ty, state, entry.priority, entry.cpu, cpu_ms, name
            );
        }
        // Statuses of exited threads that nobody has collected yet. A number
        // that only grows means something is leaking exit records.
        let _ = writeln!(
            table,
            "\npending exit statuses: {}   init pid: {}",
            crate::thread::thread::EXITED_THREADS.pending(),
            crate::thread::thread::init_pid()
        );
        table
    }

    fn render_meminfo() -> String {
        let stats = {
            let allocator = frame_allocator();
            allocator.stats()
        };

        let frame_size_bytes = 4096u64;
        let total_bytes = stats.total_frames as u64 * frame_size_bytes;
        let free_bytes = stats.free_frames as u64 * frame_size_bytes;
        let used_bytes = total_bytes.saturating_sub(free_bytes);
        let used_frames = stats.total_frames.saturating_sub(stats.free_frames);

        format!(
            concat!(
                "MemTotal: {} KiB\n",
                "MemFree: {} KiB\n",
                "MemUsed: {} KiB\n",
                "PageSize: 4096 B\n",
                "FramesTotal: {}\n",
                "FramesFree: {}\n",
                "FramesUsed: {}\n"
            ),
            total_bytes / 1024,
            free_bytes / 1024,
            used_bytes / 1024,
            stats.total_frames,
            stats.free_frames,
            used_frames,
        )
    }

    fn render_evict_stats() -> String {
        use crate::fs::evict::{EVICT_DROPPED_COUNT, evict_kthread_drain_count};
        format!(
            "drain_count: {}\ndropped_count: {}\n",
            evict_kthread_drain_count(),
            EVICT_DROPPED_COUNT.load(Ordering::Relaxed),
        )
    }

    fn render_inflight_stats() -> String {
        use crate::fs::page_fill::{
            INFLIGHT_CANCELS, INFLIGHT_CURRENT, INFLIGHT_INSTALLS, INFLIGHT_JOINS, INFLIGHT_RETRIES,
        };
        format!(
            "installs: {}\njoins: {}\nretries: {}\ncancels: {}\ncurrent: {}\n",
            INFLIGHT_INSTALLS.load(Ordering::Relaxed),
            INFLIGHT_JOINS.load(Ordering::Relaxed),
            INFLIGHT_RETRIES.load(Ordering::Relaxed),
            INFLIGHT_CANCELS.load(Ordering::Relaxed),
            INFLIGHT_CURRENT.load(Ordering::Relaxed),
        )
    }

    fn render_ahci_stats() -> String {
        use crate::drivers::ahci::watchdog::{WATCHDOG_FIRINGS, WATCHDOG_RESTARTS};
        let firings = WATCHDOG_FIRINGS.load(Ordering::Relaxed);
        let restarts = WATCHDOG_RESTARTS.load(Ordering::Relaxed);
        format!("firings={firings} restarts={restarts}\n")
    }

    fn render_block_cache() -> String {
        if !BlockPageCache::initialized() {
            return concat!(
                "hits: 0\nmisses: 0\nevictions: 0\ndetached_fallbacks: 0\n",
                "dirty_pages: 0\nwriteback_runs: 0\nwriteback_bytes: 0\n",
                "sync_calls: 0\nflush_requested: 0\nflush_completed: 0\n"
            )
            .to_string();
        }
        let s = BlockPageCache::global().stats();
        format!(
            concat!(
                "hits: {}\nmisses: {}\nevictions: {}\ndetached_fallbacks: {}\n",
                "dirty_pages: {}\nwriteback_runs: {}\nwriteback_bytes: {}\n",
                "sync_calls: {}\nflush_requested: {}\nflush_completed: {}\n"
            ),
            s.hits,
            s.misses,
            s.evictions,
            s.detached_fallbacks,
            s.dirty_pages,
            s.writeback_runs,
            s.writeback_bytes,
            s.sync_calls,
            s.flush_requested,
            s.flush_completed,
        )
    }

    fn render_lock_order_stats() -> String {
        use crate::debug::lock_order::{LOCK_ORDER_INVERSIONS, MAX_RANK_DEPTH};
        use core::sync::atomic::Ordering;

        // Both counters are only incremented in debug builds; they stay 0 in
        // release builds.  The `/proc` file always exists so userspace tooling
        // does not need to special-case the build profile.
        let inversions = LOCK_ORDER_INVERSIONS.load(Ordering::Relaxed);
        let max_depth = MAX_RANK_DEPTH.load(Ordering::Relaxed);
        format!("inversions: {inversions}\nmax_depth: {max_depth}\n")
    }

    fn resolve_path(path: &Path) -> Result<ProcNode, Error> {
        if path.is_root() {
            return Ok(ProcNode::Root);
        }

        let components = path.components();

        match components.len() {
            1 => match components[0].as_str() {
                "processes" => Ok(ProcNode::ProcessesFile),
                "meminfo" => Ok(ProcNode::MemInfo),
                "block_cache" => Ok(ProcNode::BlockCacheStats),
                "evict_stats" => Ok(ProcNode::EvictStats),
                "lock_order_stats" => Ok(ProcNode::LockOrderStats),
                "inflight_stats" => Ok(ProcNode::InflightStats),
                "ahci_stats" => Ok(ProcNode::AhciStats),
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
    fn list_files(&self, path: &Path) -> Result<Vec<File>, Error> {
        let path = path.normalize();
        match Self::resolve_path(&path)? {
            ProcNode::Root => {
                let snapshots = Self::collect_snapshots();
                let mut files = Vec::with_capacity(snapshots.len() + 3);

                let summary = Self::render_process_table(&snapshots);
                files.push(Self::file_entry("processes".to_string(), summary.len()));

                let meminfo = Self::render_meminfo();
                files.push(Self::file_entry("meminfo".to_string(), meminfo.len()));

                let block_cache = Self::render_block_cache();
                files.push(Self::file_entry(
                    "block_cache".to_string(),
                    block_cache.len(),
                ));

                let evict_stats = Self::render_evict_stats();
                files.push(Self::file_entry(
                    "evict_stats".to_string(),
                    evict_stats.len(),
                ));

                let lock_order_stats = Self::render_lock_order_stats();
                files.push(Self::file_entry(
                    "lock_order_stats".to_string(),
                    lock_order_stats.len(),
                ));

                let inflight_stats = Self::render_inflight_stats();
                files.push(Self::file_entry(
                    "inflight_stats".to_string(),
                    inflight_stats.len(),
                ));

                let ahci_stats = Self::render_ahci_stats();
                files.push(Self::file_entry("ahci_stats".to_string(), ahci_stats.len()));

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
            ProcNode::ProcessesFile
            | ProcNode::MemInfo
            | ProcNode::BlockCacheStats
            | ProcNode::EvictStats
            | ProcNode::LockOrderStats
            | ProcNode::InflightStats
            | ProcNode::AhciStats
            | ProcNode::ProcessStatus(_)
            | ProcNode::ProcessCmdline(_) => Err(Error::NotADir),
        }
    }

    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
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
            ProcNode::MemInfo => {
                let content = Self::render_meminfo();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::BlockCacheStats => {
                let content = Self::render_block_cache();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::EvictStats => {
                let content = Self::render_evict_stats();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::LockOrderStats => {
                let content = Self::render_lock_order_stats();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::InflightStats => {
                let content = Self::render_inflight_stats();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::AhciStats => {
                let content = Self::render_ahci_stats();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::Root | ProcNode::ProcessDir(_) => Err(Error::NotAFile),
        }
    }

    fn write_bytes(&self, _path: &Path, _offset: usize, _data: &[u8]) -> Result<u64, Error> {
        Err(Error::Unsupported)
    }

    fn create_file(&self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn create_dir(&self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn remove_dir(&self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn remove_file(&self, _path: &Path) -> Result<(), Error> {
        Err(Error::Unsupported)
    }

    fn file_info(&self, path: &Path) -> Result<File, Error> {
        let path = path.normalize();
        match Self::resolve_path(&path)? {
            ProcNode::Root => Ok(Self::dir_entry(String::new())),
            ProcNode::ProcessesFile => {
                let snapshots = Self::collect_snapshots();
                let summary = Self::render_process_table(&snapshots);
                Ok(Self::file_entry("processes".to_string(), summary.len()))
            }
            ProcNode::MemInfo => {
                let meminfo = Self::render_meminfo();
                Ok(Self::file_entry("meminfo".to_string(), meminfo.len()))
            }
            ProcNode::BlockCacheStats => {
                let content = Self::render_block_cache();
                Ok(Self::file_entry("block_cache".to_string(), content.len()))
            }
            ProcNode::EvictStats => {
                let content = Self::render_evict_stats();
                Ok(Self::file_entry("evict_stats".to_string(), content.len()))
            }
            ProcNode::LockOrderStats => {
                let content = Self::render_lock_order_stats();
                Ok(Self::file_entry(
                    "lock_order_stats".to_string(),
                    content.len(),
                ))
            }
            ProcNode::InflightStats => {
                let content = Self::render_inflight_stats();
                Ok(Self::file_entry(
                    "inflight_stats".to_string(),
                    content.len(),
                ))
            }
            ProcNode::AhciStats => {
                let content = Self::render_ahci_stats();
                Ok(Self::file_entry("ahci_stats".to_string(), content.len()))
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

    fn flush(&self) -> Result<(), Error> {
        Ok(())
    }

    fn statfs(&self) -> Result<super::StatFs, Error> {
        let snapshots = Self::collect_snapshots();
        let mut volume_name = [0u8; 64];
        volume_name[..5].copy_from_slice(b"proc\0");
        Ok(super::StatFs {
            fs_type: "procfs",
            block_size: 0,
            total_blocks: 0,
            free_blocks: 0,
            total_inodes: snapshots.len() as u64,
            free_inodes: 0,
            volume_name,
            version: 0,
            block_groups: 0,
        })
    }
}

#[derive(Debug, Clone)]
struct ThreadSnapshot {
    tid: u64,
    parent: u64,
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
    vma_count: Option<usize>,
    next_mmap_addr: Option<u64>,
    errno: Option<Errno>,
}

impl ThreadSnapshot {
    fn from_thread(thread: &Thread) -> Self {
        let tid = thread.id.0;
        let parent = thread.parent.load(Ordering::Acquire);
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

        let (user_pid, heap_break, vma_count) = thread
            .user
            .as_ref()
            .map(|user_arc| {
                let user = user_arc.read();
                (
                    Some(user.pid),
                    Some(user.heap_break),
                    Some(ranked_lock!(RANK_VMAS, "user.vmas", user.vmas).len()),
                )
            })
            .unwrap_or((None, None, None));

        let (user_id, group_id, cwd, errno, next_mmap_addr) = read_thread_info(thread.id);

        Self {
            tid,
            parent,
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
            vma_count,
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
            "VMAs: {}",
            display_option_decimal(self.vma_count.map(|v| v as u64))
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
    Option<u64>,
) {
    if let Some(info_arc) = get_thread_info_by_id(tid)
        && let Some(info) = info_arc.try_lock()
    {
        return (
            Some(info.user_id),
            Some(info.group_id),
            Some(info.cwd.lock().to_string()),
            Some(info.errno),
            Some(
                info.next_mmap_addr
                    .load(core::sync::atomic::Ordering::Relaxed),
            ),
        );
    }
    (None, None, None, None, None)
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
    MemInfo,
    BlockCacheStats,
    EvictStats,
    LockOrderStats,
    InflightStats,
    AhciStats,
    ProcessDir(u64),
    ProcessStatus(u64),
    ProcessCmdline(u64),
}
