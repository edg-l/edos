//! Process filesystem exposing live kernel thread information.

use core::{fmt::Write, sync::atomic::Ordering};

use alloc::{
    collections::btree_map::BTreeMap,
    format,
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};

use crate::{
    debug::lock_order::{RANK_SOCKET, RANK_USER_MM, RANK_VMAS},
    fs::{
        Error, File, FileAttrs, FileKind, FileSystem, block_page_cache::BlockPageCache, path::Path,
    },
    memory::{
        frame_allocator::frame_allocator,
        vma::{Vma, VmaBacking, VmaFlags, VmaProt},
    },
    net::socket::{SOCK_STREAM, SocketAddr, SocketState},
    ranked_lock, smp,
    syscalls::Errno,
    thread::{
        pipe::{FileDescriptor, OpenMode, StandardStream},
        thread::{State, Thread, ThreadId, get_thread_info_by_id, list_threads},
    },
};

pub struct Procfs;

impl Procfs {
    pub fn new() -> Result<Self, Error> {
        Ok(Self)
    }

    fn collect_snapshots() -> Vec<ThreadSnapshot> {
        // Threads of one process share an address space, so without this every
        // thread of an N-threaded process walks the same page tables again,
        // with preemption suppressed for the whole of each walk. Keyed by the
        // shared `Arc`'s address, which is what "same address space" means
        // here.
        let mut resident = BTreeMap::new();
        let mut snapshots: Vec<ThreadSnapshot> = list_threads()
            .into_iter()
            .map(|thread| ThreadSnapshot::from_thread(thread.as_ref(), &mut resident))
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
        let mut table =
            String::from("PID   PPID  PGID  TYPE   STATE     PRIO CPU CPUms RSSKiB NAME\n");
        for entry in entries {
            let ty = if entry.is_kernel { "kernel" } else { "user" };
            let state = if entry.stopped {
                "Stopped".to_string()
            } else {
                format!("{:?}", entry.state)
            };
            let name = entry.display_name();
            let cpu_ms = entry.cpu_time_ns / 1_000_000;
            // A kernel thread has no address space of its own, so it reports
            // no resident size rather than the kernel's.
            let rss_kib = entry
                .resident
                .map(|bytes| format!("{}", bytes / 1024))
                .unwrap_or_else(|| "-".to_string());
            let _ = writeln!(
                table,
                "{:<5} {:<5} {:<5} {:<6} {:<9} {:<4} {:<3} {:>6} {:>6} {}",
                entry.tid,
                entry.parent,
                entry.pgid,
                ty,
                state,
                entry.priority,
                entry.cpu,
                cpu_ms,
                rss_kib,
                name
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

    fn render_cpuinfo() -> String {
        use raw_cpuid::CpuId;

        let cpuid = CpuId::new();
        let vendor = cpuid
            .get_vendor_info()
            .map(|v| v.as_str().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let brand = cpuid
            .get_processor_brand_string()
            .map(|b| b.as_str().trim().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let feature = cpuid.get_feature_info();
        let extended = cpuid.get_extended_feature_info();

        // Only the flags the kernel itself depends on or reports elsewhere.
        let mut flags = String::new();
        if let Some(f) = feature.as_ref() {
            for (name, present) in [
                ("fpu", f.has_fpu()),
                ("tsc", f.has_tsc()),
                ("msr", f.has_msr()),
                ("pae", f.has_pae()),
                ("apic", f.has_apic()),
                ("sse", f.has_sse()),
                ("sse2", f.has_sse2()),
                ("sse3", f.has_sse3()),
                ("ssse3", f.has_ssse3()),
                ("sse4_1", f.has_sse41()),
                ("sse4_2", f.has_sse42()),
                ("x2apic", f.has_x2apic()),
                ("aes", f.has_aesni()),
                ("xsave", f.has_xsave()),
                ("avx", f.has_avx()),
                ("rdrand", f.has_rdrand()),
                ("hypervisor", f.has_hypervisor()),
            ] {
                if present {
                    if !flags.is_empty() {
                        flags.push(' ');
                    }
                    flags.push_str(name);
                }
            }
        }
        if let Some(e) = extended.as_ref() {
            for (name, present) in [
                ("fsgsbase", e.has_fsgsbase()),
                ("smep", e.has_smep()),
                ("smap", e.has_smap()),
                ("avx2", e.has_avx2()),
                ("rdseed", e.has_rdseed()),
            ] {
                if present {
                    if !flags.is_empty() {
                        flags.push(' ');
                    }
                    flags.push_str(name);
                }
            }
        }
        if flags.is_empty() {
            flags.push_str("unknown");
        }

        let online = smp::cpu_count();
        let detected = smp::NUM_CPUS.load(Ordering::Relaxed).max(online);

        let mut out = String::new();
        for index in 0..online {
            let _ = writeln!(out, "processor: {index}");
            let _ = writeln!(out, "apicid: {}", smp::lapic_id_for_cpu(index));
            let _ = writeln!(out, "vendor_id: {vendor}");
            let _ = writeln!(out, "model name: {brand}");
            if let Some(f) = feature.as_ref() {
                let _ = writeln!(out, "cpu family: {}", f.family_id());
                let _ = writeln!(out, "model: {}", f.model_id());
                let _ = writeln!(out, "stepping: {}", f.stepping_id());
            }
            let _ = writeln!(out, "flags: {flags}");
            out.push('\n');
        }

        let _ = writeln!(out, "cpus detected: {detected}");
        let _ = writeln!(out, "cpus online: {online}");
        out
    }

    fn render_evict_stats() -> String {
        use crate::fs::evict::{
            EVICT_DROPPED_COUNT, EVICT_SYNC_FALLBACK_COUNT, evict_kthread_drain_count,
        };
        format!(
            "drain_count: {}\ndropped_count: {}\nsync_fallback_count: {}\n",
            evict_kthread_drain_count(),
            EVICT_DROPPED_COUNT.load(Ordering::Relaxed),
            EVICT_SYNC_FALLBACK_COUNT.load(Ordering::Relaxed),
        )
    }

    /// Which of the four paths each readahead window past the caller's request
    /// took. Only `async_windows` is asynchronous; two are a bulk fill the
    /// reader pays for inside its own `read`, so a pass dominated by them says
    /// nothing about whether prefetch trails the reader; `skipped_windows` is
    /// the window an earlier one already covers.
    fn render_readahead_stats() -> String {
        use crate::fs::readahead::{
            RA_ASYNC_DROPPED_PAGES, RA_ASYNC_DROPPED_WINDOWS, RA_ASYNC_PAGES, RA_ASYNC_WINDOWS,
            RA_ERR_PAGES, RA_ERR_WINDOWS, RA_SKIPPED_PAGES, RA_SKIPPED_WINDOWS, RA_SYNC_PAGES,
            RA_SYNC_WINDOWS, RA_TRIMMED_PAGES, RA_TRIMMED_WINDOWS,
        };
        format!(
            "async_windows: {}\nasync_pages: {}\n\
             async_dropped_windows: {}\nasync_dropped_pages: {}\n\
             sync_windows: {}\nsync_pages: {}\nerr_windows: {}\nerr_pages: {}\n\
             skipped_windows: {}\nskipped_pages: {}\n\
             trimmed_windows: {}\ntrimmed_pages: {}\n",
            RA_ASYNC_WINDOWS.load(Ordering::Relaxed),
            RA_ASYNC_PAGES.load(Ordering::Relaxed),
            RA_ASYNC_DROPPED_WINDOWS.load(Ordering::Relaxed),
            RA_ASYNC_DROPPED_PAGES.load(Ordering::Relaxed),
            RA_SYNC_WINDOWS.load(Ordering::Relaxed),
            RA_SYNC_PAGES.load(Ordering::Relaxed),
            RA_ERR_WINDOWS.load(Ordering::Relaxed),
            RA_ERR_PAGES.load(Ordering::Relaxed),
            RA_SKIPPED_WINDOWS.load(Ordering::Relaxed),
            RA_SKIPPED_PAGES.load(Ordering::Relaxed),
            RA_TRIMMED_WINDOWS.load(Ordering::Relaxed),
            RA_TRIMMED_PAGES.load(Ordering::Relaxed),
        )
    }

    fn render_efs_stats() -> String {
        use crate::fs::efs::{EFS_ALLOC_FAILED, EFS_BLOCKS_ALLOCATED, EFS_BLOCKS_FREED};
        use crate::fs::efs::{EFS_EXTENT_BATCHES, EFS_EXTENT_READS, EFS_EXTENT_RUNS};
        use crate::fs::inode::{ORPHANS_DROPPED, ORPHANS_MARKED};
        use crate::fs::journal::tx::TX_ABORTS;
        format!(
            "blocks_allocated: {}\nblocks_freed: {}\nalloc_failed: {}\ntx_aborts: {}\n\
             orphans_marked: {}\norphans_dropped: {}\n\
             extent_reads: {}\nextent_runs: {}\nextent_batches: {}\n",
            EFS_BLOCKS_ALLOCATED.load(Ordering::Relaxed),
            EFS_BLOCKS_FREED.load(Ordering::Relaxed),
            EFS_ALLOC_FAILED.load(Ordering::Relaxed),
            TX_ABORTS.load(Ordering::Relaxed),
            ORPHANS_MARKED.load(Ordering::Relaxed),
            ORPHANS_DROPPED.load(Ordering::Relaxed),
            EFS_EXTENT_READS.load(Ordering::Relaxed),
            EFS_EXTENT_RUNS.load(Ordering::Relaxed),
            EFS_EXTENT_BATCHES.load(Ordering::Relaxed),
        )
    }

    fn render_journal_stats() -> String {
        use crate::fs::journal::{
            JOURNAL_CHECKPOINTS, JOURNAL_COMMANDS, JOURNAL_COMMIT_US, JOURNAL_COMMITS,
            JOURNAL_DATA_BLOCKS, JOURNAL_EMPTY_COMMITS, JOURNAL_FLUSH_US, JOURNAL_RING_BLOCKS,
            JOURNAL_RING_US,
        };
        let commits = JOURNAL_COMMITS.load(Ordering::Relaxed);
        let ring_blocks = JOURNAL_RING_BLOCKS.load(Ordering::Relaxed);
        let commands = JOURNAL_COMMANDS.load(Ordering::Relaxed);
        let ring_us = JOURNAL_RING_US.load(Ordering::Relaxed);
        let flush_us = JOURNAL_FLUSH_US.load(Ordering::Relaxed);
        let commit_us = JOURNAL_COMMIT_US.load(Ordering::Relaxed);
        // Averages are the point of the file: a commit's cost is what the
        // fsync-per-write row of fsbench pays, and blocks per command is how
        // much of the batch the drive sees at once.
        let per = |total: u64| if commits == 0 { 0 } else { total / commits };
        format!(
            concat!(
                "commits: {}\nempty_commits: {}\ncheckpoints: {}\n",
                "ring_blocks: {}\ndata_blocks: {}\ncommands: {}\n",
                "ring_us: {}\nflush_us: {}\ncommit_us: {}\n",
                "us_per_commit: {}\nblocks_per_commit: {}\nblocks_per_command: {}\n"
            ),
            commits,
            JOURNAL_EMPTY_COMMITS.load(Ordering::Relaxed),
            JOURNAL_CHECKPOINTS.load(Ordering::Relaxed),
            ring_blocks,
            JOURNAL_DATA_BLOCKS.load(Ordering::Relaxed),
            commands,
            ring_us,
            flush_us,
            commit_us,
            per(ring_us + flush_us + commit_us),
            per(ring_blocks),
            // The commit block is its own FUA command and is not part of the
            // batch, so it comes off both sides of this ratio.
            if commands == 0 {
                0
            } else {
                ring_blocks.saturating_sub(commits) / commands
            },
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
        use crate::drivers::ahci::watchdog::{
            NCQ_INFLIGHT, NCQ_MAX_INFLIGHT, NCQ_STRANDED, NCQ_TIMEOUT_MS, WATCHDOG_FIRINGS,
            WATCHDOG_RESTARTS,
        };
        let firings = WATCHDOG_FIRINGS.load(Ordering::Relaxed);
        let restarts = WATCHDOG_RESTARTS.load(Ordering::Relaxed);
        let stranded = NCQ_STRANDED.load(Ordering::Relaxed);
        let timeout_ms = NCQ_TIMEOUT_MS.load(Ordering::Relaxed);
        let inflight = NCQ_INFLIGHT.load(Ordering::Relaxed);
        let max_inflight = NCQ_MAX_INFLIGHT.load(Ordering::Relaxed);
        format!(
            "firings={firings} restarts={restarts} stranded={stranded} timeout_ms={timeout_ms} \
             ncq_inflight={inflight} ncq_max_inflight={max_inflight}\n"
        )
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

    /// The window registry, one line per window.
    ///
    /// Columns are fixed and the title comes last, because it is the only field
    /// that can contain a space. `X`/`Y` are the outer origin, `W`/`H` the
    /// client size and `FRAME` the manager's border, so the centre of a
    /// window's client area is `(X + frame.left + W/2, Y + frame.top + H/2)` --
    /// which is what lets something outside the shell address a window by name
    /// instead of by a pixel copied out of the panel's layout.
    fn render_windows() -> String {
        let (windows, focused) = crate::window::registry::snapshot();
        let mut out = String::from(
            "ID    PID   X      Y      W     H     Z     FLAGS FRAME       STATE     TITLE\n",
        );
        for w in windows {
            let state = if focused == Some(w.id) {
                "focused"
            } else if w.minimized {
                "minimized"
            } else if !w.visible {
                "unmapped"
            } else {
                "normal"
            };
            let frame = format!(
                "{},{},{},{}",
                w.frame.left, w.frame.top, w.frame.right, w.frame.bottom
            );
            let _ = writeln!(
                out,
                "{:<5} {:<5} {:<6} {:<6} {:<5} {:<5} {:<5} {:<5} {:<11} {:<9} {}",
                w.id, w.pid, w.x, w.y, w.width, w.height, w.z_order, w.flags, frame, state, w.title
            );
        }
        out
    }

    /// Interface state as `key: value`, for a program that wants to *use* the
    /// numbers. `SYS_NETINFO` renders the same state for a terminal, colour
    /// codes and all, which is the wrong thing to hand a parser.
    fn render_net() -> String {
        use crate::{debug::lock_order::RANK_NET_STACK, net::device::NetDevice};
        let mut out = String::from("interface: lo\nlink: up\ninet: 127.0.0.1\nprefix: 8\n\n");
        let Some(stack_mutex) = crate::net::stack::NET_STACK.get() else {
            let _ = writeln!(out, "interface: none");
            return out;
        };
        let stack = ranked_lock!(RANK_NET_STACK, "procfs::net", stack_mutex);
        let mac = stack.mac();
        let ip = stack.local_ip;
        let gw = stack.gateway_ip;
        let dns = stack.dns_server;
        let prefix: u32 = stack.subnet_mask.iter().map(|b| b.count_ones()).sum();
        let link = stack.nic.link_up();
        drop(stack);

        let _ = writeln!(out, "interface: eth0");
        let _ = writeln!(out, "link: {}", if link { "up" } else { "down" });
        let _ = writeln!(
            out,
            "mac: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        );
        let _ = writeln!(out, "inet: {}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]);
        let _ = writeln!(out, "prefix: {prefix}");
        let _ = writeln!(out, "gateway: {}.{}.{}.{}", gw[0], gw[1], gw[2], gw[3]);
        let _ = writeln!(out, "dns: {}.{}.{}.{}", dns[0], dns[1], dns[2], dns[3]);
        out
    }

    /// Every TCP connection the stack is tracking, then every port-table
    /// binding that has no connection of its own: listening TCP sockets and
    /// bound UDP ports.
    ///
    /// `RECVQ` is what has arrived and not been read yet; `SENDQ` is what has
    /// been sent and not acknowledged. A connection appears here for as long
    /// as the stack tracks it, so a `TIME_WAIT` with no descriptor left still
    /// shows, which is the whole reason a connection list is not derivable
    /// from `/proc/<tid>/fd`.
    ///
    /// This is `/proc/sockets` and not `/proc/net/tcp`: `/proc/net` is already
    /// a file, and procfs has no directories other than one per thread.
    fn render_sockets() -> String {
        use crate::{
            debug::lock_order::{RANK_NET_STACK, RANK_PORT_TABLE, RANK_TCP_CONN},
            net::{socket::port_table, stack::NET_STACK},
        };

        let mut out = String::from("PROTO RECVQ SENDQ LOCAL FOREIGN STATE\n");

        // Both tables are snapshotted and released before any socket or
        // connection is locked. The receive path takes NET_STACK (240) and
        // PORT_TABLE (250) above Socket (260) and TcpConnection (270), so
        // holding either across the object it dispatches to is legal by rank
        // but pins the whole stack behind one `cat`.
        let connections = match NET_STACK.get() {
            Some(stack) => {
                let stack = ranked_lock!(RANK_NET_STACK, "procfs::sockets", stack);
                stack.tcp_connections.values().cloned().collect::<Vec<_>>()
            }
            None => Vec::new(),
        };
        let bound = {
            let table = ranked_lock!(RANK_PORT_TABLE, "procfs::sockets", port_table());
            table.values().cloned().collect::<Vec<_>>()
        };

        for connection in connections {
            let connection = ranked_lock!(RANK_TCP_CONN, "procfs::sockets", connection);
            let _ = writeln!(
                out,
                "tcp {} {} {} {} {}",
                connection.rx_buffer.len(),
                connection.snd_nxt.wrapping_sub(connection.snd_una),
                display_endpoint(connection.local_ip, connection.local_port),
                display_endpoint(connection.remote_ip, connection.remote_port),
                connection.state.name()
            );
        }

        for socket in bound {
            let socket = ranked_lock!(RANK_SOCKET, "procfs::sockets", socket);
            // A connected TCP socket is already listed from the connection
            // table, which knows its sequence space; only bindings without one
            // are left to report.
            if socket.sock_type == SOCK_STREAM && socket.tcp_conn.is_some() {
                continue;
            }
            let (proto, state) = if socket.sock_type == SOCK_STREAM {
                ("tcp", if socket.listening { "LISTEN" } else { "BOUND" })
            } else {
                ("udp", "-")
            };
            let _ = writeln!(
                out,
                "{proto} {} 0 {} {} {state}",
                socket
                    .rx_queue
                    .iter()
                    .map(|(data, _)| data.len())
                    .sum::<usize>(),
                display_socket_addr(socket.local_addr),
                display_socket_addr(socket.remote_addr)
            );
        }

        out
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

    /// Render one per-thread file, or `FileNotFound` if no such thread is alive.
    fn with_snapshot(tid: u64, render: fn(&ThreadSnapshot) -> String) -> Result<String, Error> {
        let snapshots = Self::collect_snapshots();
        Self::find_snapshot(&snapshots, tid)
            .map(render)
            .ok_or(Error::FileNotFound)
    }

    /// One line per VMA: the address range, its protection, the size asked for
    /// and the part of it that is actually mapped, then what backs it.
    ///
    /// Rendered straight from the thread rather than from a `ThreadSnapshot`,
    /// because every other `/proc` reader would then pay for a page-table walk
    /// per mapping to build a field nothing else uses.
    fn render_maps(tid: u64) -> Result<String, Error> {
        let thread = list_threads()
            .into_iter()
            .find(|thread| thread.id.0 == tid)
            .ok_or(Error::FileNotFound)?;
        let mut out = String::from("START-END PERM SIZEKIB RSSKIB BACKING\n");
        // A kernel thread has no address space of its own; the file exists so
        // readers need not special-case it, and lists nothing.
        let Some(user_arc) = thread.user.as_ref() else {
            return Ok(out);
        };
        let user = user_arc.read();

        // Copied out under the VMA lock so residency, which takes the memory
        // manager (rank 80), is computed after it is released.
        let entries: Vec<(u64, u64, String, String)> = {
            let vmas = ranked_lock!(RANK_VMAS, "procfs::maps", user.vmas);
            vmas.iter()
                .map(|vma| {
                    (
                        vma.start.as_u64(),
                        vma.end.as_u64(),
                        vma_prot_text(vma),
                        vma_backing_text(vma),
                    )
                })
                .collect()
        };

        let manager = ranked_lock!(RANK_USER_MM, "procfs::maps_resident", user.memory_manager);
        for (start, end, prot, backing) in entries {
            let resident = manager.resident_bytes_in(start, end) / 1024;
            let _ = writeln!(
                out,
                "{start:016x}-{end:016x} {prot} {} {resident} {backing}",
                (end - start) / 1024
            );
        }
        Ok(out)
    }

    /// `/proc/<tid>/fd`: the thread's open descriptors, one per line.
    ///
    /// A directory of symbolic links is what Linux offers; this is a table
    /// instead, because a descriptor here is not always a path — a pipe end, a
    /// PTY side and a socket have no name in the filesystem, and the fields
    /// that identify them (the shared object's address, the connection's
    /// endpoints) do not fit in a link target.
    fn render_fds(tid: u64) -> Result<String, Error> {
        let thread = list_threads()
            .into_iter()
            .find(|thread| thread.id.0 == tid)
            .ok_or(Error::FileNotFound)?;
        let mut out = String::from("FD TYPE MODE POS NAME\n");
        // A kernel thread owns no descriptor table; the file exists so readers
        // need not special-case it, and lists nothing.
        if thread.user.is_none() {
            return Ok(out);
        }

        // The table handle leaves the thread-info spinlock before it is
        // locked: that lock runs with interrupts off, and the table is a
        // BlockingMutex whose contended acquisition parks.
        //
        // The acquisition is a bounded spin on `try_lock` rather than `lock`,
        // because a thread reading its own `/proc/<tid>/fd` can already hold
        // this lock further up the syscall path and would deadlock on itself.
        // Losing the race is `Busy`, not `FileNotFound`: a contended descriptor
        // table is not a thread that has exited, and telling a reader the file
        // does not exist is a lie it cannot distinguish from the real thing.
        let table = {
            let info = get_thread_info_by_id(thread.id).ok_or(Error::FileNotFound)?;
            let mut acquired = None;
            for _ in 0..FD_TABLE_LOCK_SPINS {
                if let Some(guard) = info.try_lock() {
                    acquired = Some(guard.fd_table.clone());
                    break;
                }
                core::hint::spin_loop();
            }
            acquired.ok_or(Error::Busy)?
        };

        // Cloned out under the table lock and rendered after it is released:
        // describing a socket takes the socket lock (rank 260), and a
        // descriptor clone shares the underlying object without touching the
        // open counts, which only close adjusts.
        let entries: Vec<(u64, FileDescriptor)> = {
            let guard = table.lock();
            guard.iter_all().map(|(fd, d)| (fd, d.clone())).collect()
        };

        for (fd, descriptor) in entries {
            let (kind, mode, pos, name) = describe_descriptor(&descriptor);
            let _ = writeln!(out, "{fd} {kind} {mode} {pos} {name}");
        }
        Ok(out)
    }

    fn resolve_path(path: &Path) -> Result<ProcNode, Error> {
        if path.is_root() {
            return Ok(ProcNode::Root);
        }

        let components = path.components();

        match components.len() {
            1 => match GLOBAL_FILES
                .iter()
                .position(|(name, _)| *name == components[0].as_str())
            {
                Some(index) => Ok(ProcNode::GlobalFile(index)),
                None => parse_tid(&components[0])
                    .map(ProcNode::ProcessDir)
                    .ok_or(Error::FileNotFound),
            },
            2 => {
                let tid = parse_tid(&components[0]).ok_or(Error::FileNotFound)?;
                PROCESS_FILES
                    .iter()
                    .position(|(name, _)| *name == components[1].as_str())
                    .map(|index| ProcNode::ProcessFile(tid, index))
                    .ok_or(Error::FileNotFound)
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
                let mut files = Vec::with_capacity(snapshots.len() + GLOBAL_FILES.len());

                for (name, render) in GLOBAL_FILES {
                    files.push(Self::file_entry(name.to_string(), render().len()));
                }

                for snapshot in snapshots {
                    files.push(Self::dir_entry(snapshot.tid.to_string()));
                }

                Ok(files)
            }
            ProcNode::ProcessDir(tid) => {
                let snapshots = Self::collect_snapshots();
                if Self::find_snapshot(&snapshots, tid).is_none() {
                    return Err(Error::FileNotFound);
                }

                Ok(PROCESS_FILES
                    .iter()
                    .map(|(name, render)| {
                        Self::file_entry(
                            name.to_string(),
                            render(tid).map(|text| text.len()).unwrap_or(0),
                        )
                    })
                    .collect())
            }
            ProcNode::GlobalFile(_) | ProcNode::ProcessFile(..) => Err(Error::NotADir),
        }
    }

    fn read_bytes(&self, path: &Path, offset: usize, count: usize) -> Result<Vec<u8>, Error> {
        let path = path.normalize();
        match Self::resolve_path(&path)? {
            ProcNode::GlobalFile(index) => {
                let content = GLOBAL_FILES[index].1();
                Ok(Self::read_text(content, offset, count))
            }
            ProcNode::ProcessFile(tid, index) => {
                let content = PROCESS_FILES[index].1(tid)?;
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
            ProcNode::GlobalFile(index) => {
                let (name, render) = GLOBAL_FILES[index];
                Ok(Self::file_entry(name.to_string(), render().len()))
            }
            ProcNode::ProcessDir(tid) => {
                let snapshots = Self::collect_snapshots();
                if Self::find_snapshot(&snapshots, tid).is_none() {
                    Err(Error::FileNotFound)
                } else {
                    Ok(Self::dir_entry(path.filename()))
                }
            }
            ProcNode::ProcessFile(tid, index) => {
                let (name, render) = PROCESS_FILES[index];
                let content = render(tid)?;
                Ok(Self::file_entry(name.to_string(), content.len()))
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
    pgid: u64,
    name: String,
    state: State,
    /// Suspended by a stop signal. Reported instead of `state`, which for a
    /// stopped thread only says it is parked and not why.
    stopped: bool,
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
    /// The command line of a user thread's image, arguments included. A kernel
    /// thread has none, and reports its name instead.
    cmdline: Option<String>,
    user_id: Option<u32>,
    group_id: Option<u32>,
    cwd: Option<String>,
    heap_break: Option<u64>,
    vma_count: Option<usize>,
    vm_size: Option<u64>,
    resident: Option<u64>,
    next_mmap_addr: Option<u64>,
    errno: Option<Errno>,
}

impl ThreadSnapshot {
    fn from_thread(thread: &Thread, resident_cache: &mut BTreeMap<usize, u64>) -> Self {
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
        let pgid = thread.pgid();
        let stopped = thread.stopped.load(Ordering::Acquire);

        let (user_pid, cmdline, heap_break, vma_count, vm_size, resident) = thread
            .user
            .as_ref()
            .map(|user_arc| {
                let user = user_arc.read();
                let (vma_count, vm_size) = {
                    let vmas = ranked_lock!(RANK_VMAS, "user.vmas", user.vmas);
                    (vmas.len(), vmas.iter().map(|vma| vma.size()).sum::<u64>())
                };
                // After the VMA set, never before: vmas is rank 70 and the
                // memory manager 80.
                let key = Arc::as_ptr(&user.memory_manager) as *const u8 as usize;
                let resident = match resident_cache.get(&key) {
                    Some(bytes) => *bytes,
                    None => {
                        let bytes =
                            ranked_lock!(RANK_USER_MM, "procfs::resident", user.memory_manager)
                                .resident_bytes();
                        resident_cache.insert(key, bytes);
                        bytes
                    }
                };
                (
                    Some(user.pid),
                    Some(user.cmdline.to_string()),
                    Some(user.heap_break),
                    Some(vma_count),
                    Some(vm_size),
                    Some(resident),
                )
            })
            .unwrap_or((None, None, None, None, None, None));

        let (user_id, group_id, cwd, errno, next_mmap_addr) = read_thread_info(thread.id);

        Self {
            tid,
            parent,
            name,
            pgid,
            state,
            stopped,
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
            cmdline,
            user_id,
            group_id,
            cwd,
            heap_break,
            vma_count,
            vm_size,
            resident,
            next_mmap_addr,
            errno,
        }
    }

    /// The last column of `/proc/processes`, so arguments can run to the end of
    /// the line without disturbing anything that parses the fixed columns.
    fn display_name(&self) -> &str {
        self.cmdline.as_deref().unwrap_or(self.name.as_str())
    }

    fn cmdline_text(&self) -> String {
        if self.is_kernel {
            format!("[kernel] {}\n", self.name)
        } else {
            format!("{}\n", self.display_name())
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
        let _ = writeln!(
            out,
            "State: {}",
            if self.stopped {
                "Stopped".to_string()
            } else {
                format!("{:?}", self.state)
            }
        );
        let _ = writeln!(out, "ProcessGroup: {}", self.pgid);
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
        // Address space asked for, then the part of it that has actually been
        // faulted in. Almost everything here is demand-paged, so the two are
        // far apart and only the second is memory the machine has spent.
        let _ = writeln!(
            out,
            "VM Size: {} KiB",
            display_option_decimal(self.vm_size.map(|v| v / 1024))
        );
        let _ = writeln!(
            out,
            "Resident: {} KiB",
            display_option_decimal(self.resident.map(|v| v / 1024))
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

/// `rwx` plus the sharing of the mapping, as `/proc/<pid>/maps` writes it.
fn vma_prot_text(vma: &Vma) -> String {
    let bit = |set: bool, ch: char| if set { ch } else { '-' };
    let mut text = String::with_capacity(4);
    text.push(bit(vma.prot.contains(VmaProt::READ), 'r'));
    text.push(bit(vma.prot.contains(VmaProt::WRITE), 'w'));
    text.push(bit(vma.prot.contains(VmaProt::EXEC), 'x'));
    text.push(bit(vma.flags.contains(VmaFlags::SHARED), 's'));
    if !vma.flags.contains(VmaFlags::SHARED) {
        text.pop();
        text.push('p');
    }
    text
}

/// One whitespace-free token naming what a mapping is made of. A file-backed
/// mapping is named by mount and inode number: an inode carries no path, and
/// the dentry that named it may already be gone.
fn vma_backing_text(vma: &Vma) -> String {
    match &vma.backing {
        VmaBacking::Anonymous => "anon".to_string(),
        VmaBacking::Physical { phys_base } => format!("phys:0x{phys_base:x}"),
        VmaBacking::SharedMemory { shm_id } => format!("shm:{shm_id}"),
        VmaBacking::Tls => "tls".to_string(),
        VmaBacking::Stack => "stack".to_string(),
        VmaBacking::FileBacked {
            inode,
            file_offset,
            shared,
            ..
        } => format!(
            "file:{}:{}+{file_offset}{}",
            inode.mount_id,
            inode.ino,
            if *shared { ":shared" } else { "" }
        ),
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

/// One `/proc/<tid>/fd` row's `TYPE MODE POS NAME` fields.
///
/// Objects with no name in the filesystem are identified by the address of the
/// object the descriptor shares, so the two ends of a pipe or the two sides of
/// a PTY can be matched up across processes.
fn describe_descriptor(descriptor: &FileDescriptor) -> (&'static str, &'static str, u64, String) {
    fn identity<T>(shared: &Arc<T>) -> usize {
        Arc::as_ptr(shared) as *const u8 as usize
    }

    match descriptor {
        FileDescriptor::StandardStream(stream) => match stream {
            StandardStream::Stdin => ("stream", "r", 0, "stdin".to_string()),
            StandardStream::Stdout => ("stream", "w", 0, "stdout".to_string()),
            StandardStream::Stderr => ("stream", "w", 0, "stderr".to_string()),
        },
        FileDescriptor::PipeRead(pipe) => ("pipe", "r", 0, format!("pipe:[{:x}]", identity(pipe))),
        FileDescriptor::PipeWrite(pipe) => ("pipe", "w", 0, format!("pipe:[{:x}]", identity(pipe))),
        FileDescriptor::PtyMaster(pty) => ("pty", "rw", 0, format!("ptmx:[{:x}]", identity(pty))),
        FileDescriptor::PtySlave(pty) => ("pty", "rw", 0, format!("pts:[{:x}]", identity(pty))),
        FileDescriptor::FsFile(file) => {
            let mode = match file.mode {
                OpenMode::ReadOnly => "r",
                OpenMode::WriteOnly => "w",
                OpenMode::ReadWrite => "rw",
            };
            ("file", mode, file.offset, format!("{}", file.path))
        }
        FileDescriptor::Socket(socket) => {
            let socket = ranked_lock!(RANK_SOCKET, "procfs::fd", socket);
            let proto = if socket.sock_type == SOCK_STREAM {
                "tcp"
            } else {
                "udp"
            };
            let state = if socket.listening {
                "LISTEN"
            } else {
                match socket.state {
                    SocketState::Unbound => "UNBOUND",
                    SocketState::Bound => "BOUND",
                    SocketState::Connected => "CONNECTED",
                    SocketState::Closed => "CLOSED",
                }
            };
            let name = format!(
                "{proto}:{}->{} {state}",
                display_socket_addr(socket.local_addr),
                display_socket_addr(socket.remote_addr)
            );
            ("socket", "rw", 0, name)
        }
    }
}

fn display_endpoint(ip: [u8; 4], port: u16) -> String {
    format!("{}.{}.{}.{}:{}", ip[0], ip[1], ip[2], ip[3], port)
}

fn display_socket_addr(addr: Option<SocketAddr>) -> String {
    match addr {
        Some(addr) => format!(
            "{}.{}.{}.{}:{}",
            addr.ip[0], addr.ip[1], addr.ip[2], addr.ip[3], addr.port
        ),
        None => "*:*".to_string(),
    }
}

fn display_option_errno(value: Option<Errno>) -> String {
    value
        .map(|errno| format!("{:?}", errno))
        .unwrap_or_else(|| "-".to_string())
}

#[derive(Debug, Clone, Copy)]
enum ProcNode {
    Root,
    /// Index into `GLOBAL_FILES`.
    GlobalFile(usize),
    ProcessDir(u64),
    /// Thread id, and an index into `PROCESS_FILES`.
    ProcessFile(u64, usize),
}

/// How long a `/proc/<tid>/fd` read spins for the thread-info lock before it
/// gives up with `Busy`. The lock is held for a handful of instructions
/// everywhere it is taken, so this is orders of magnitude more than enough and
/// still bounded for the self-inspection case, which can never win it.
const FD_TABLE_LOCK_SPINS: u32 = 1024;

/// The files under `/proc/<tid>/`, each rendered on demand for that thread.
/// `FileNotFound` means the thread went away between lookup and read.
///
/// A table for the same reason `GLOBAL_FILES` is one.
const PROCESS_FILES: &[(&str, fn(u64) -> Result<String, Error>)] = &[
    ("status", |tid| {
        Procfs::with_snapshot(tid, ThreadSnapshot::status_text)
    }),
    ("cmdline", |tid| {
        Procfs::with_snapshot(tid, ThreadSnapshot::cmdline_text)
    }),
    ("maps", Procfs::render_maps),
    ("fd", Procfs::render_fds),
];

/// The files directly under `/proc`, each rendered on demand.
///
/// One table rather than a variant and four match arms per file: lookup,
/// listing, read and stat all walk it, so adding a counter is one line and
/// cannot land in three of the four places.
const GLOBAL_FILES: &[(&str, fn() -> String)] = &[
    ("processes", || {
        Procfs::render_process_table(&Procfs::collect_snapshots())
    }),
    ("meminfo", Procfs::render_meminfo),
    ("cpuinfo", Procfs::render_cpuinfo),
    ("block_cache", Procfs::render_block_cache),
    ("evict_stats", Procfs::render_evict_stats),
    ("efs_stats", Procfs::render_efs_stats),
    ("journal_stats", Procfs::render_journal_stats),
    ("readahead_stats", Procfs::render_readahead_stats),
    ("lock_order_stats", Procfs::render_lock_order_stats),
    ("inflight_stats", Procfs::render_inflight_stats),
    ("ahci_stats", Procfs::render_ahci_stats),
    ("windows", Procfs::render_windows),
    ("net", Procfs::render_net),
    ("sockets", Procfs::render_sockets),
    ("syscalls", crate::syscalls::trace::render_syscall_table),
    ("sched_prof", crate::thread::sched_prof::render),
];
