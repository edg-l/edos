//! NVMe block driver (NVMe Base Specification 2.0, NVM Command Set
//! Specification 1.0).
//!
//! Structured as a direct analogue of the AHCI driver: a named kthread
//! probes PCI, brings controllers up, and identifies every namespace they
//! report, then serves as the IRQ-dispatcher thread for every controller's
//! completion queues.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::{
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use spin::Once;
use thiserror::Error;

use x86_64::{VirtAddr, structures::paging::PageTableFlags};

use crate::{
    drivers::{
        block_io::{self, AsyncBlockDevice, BlockBuffer, BlockError},
        nvme::{
            admin::{IO_QID, NvmeController},
            cancel_op::NvmeOp,
            identify::max_transfer_bytes,
            namespace::NvmeNamespace,
            queue::PRP_LIST_ENTRIES,
        },
        pci::{pci_manager, structures::PciDevice},
    },
    interrupts::io::NVME_IRQS_FIRED,
    log,
    memory::{
        mapper::memory_mapper,
        valloc::{vfree, vmalloc},
    },
    println,
    thread::{
        runqueue::IO_PRIORITY,
        scheduler::{current_thread, current_thread_weak, thread_exit, thread_park_while},
        util::{queue_spawn_kthread_named, queue_spawn_kthread_named_arg},
    },
};

pub mod admin;
pub mod api;
pub mod cancel_op;
pub mod identify;
pub mod namespace;
pub mod queue;
pub mod regs;
pub mod stats;
pub mod watchdog;

#[derive(Debug, Error, Clone, Copy)]
pub enum NvmeError {
    #[error("invalid device")]
    InvalidDevice,
    #[error(transparent)]
    DmaError(#[from] crate::drivers::dma::DmaError),
    #[error("controller timeout")]
    ControllerTimeout,
    /// The upper 16 bits of a failed completion's DW3 (Status Code Type,
    /// Status Code and related flags), as handed to `NvmeQueue::drain`.
    #[error("command failed, status={0:#x}")]
    CommandFailed(u16),
    #[error("unsupported controller")]
    Unsupported,
}

/// Set by the `nvme_probe_read` kernel cmdline flag, read once controller
/// bring-up finishes. A cmdline-gated probe rather than a permanent boot
/// path: this driver has no consumer that reads through it until the
/// device registers with `block_io`, so exercising the read path before
/// then needs its own trigger.
static NVME_PROBE_READ: AtomicBool = AtomicBool::new(false);

pub fn set_probe_read(enabled: bool) {
    NVME_PROBE_READ.store(enabled, Ordering::Relaxed);
}

/// Every controller the probe brought up, in probe order. Published
/// before the barrier below, and empty rather than absent on a machine
/// with no NVMe hardware, so the watchdog and the shutdown path can wait
/// on it unconditionally.
pub static NVME_CONTROLLERS: Once<Vec<Arc<NvmeController>>> = Once::new();

/// Every namespace the probe accepted and registered, in registration order.
pub static NVME_NAMESPACES: Once<Vec<Arc<NvmeNamespace>>> = Once::new();

/// Signalled once the probe has finished, after `NVME_NAMESPACES` is
/// published so a waiter that sees this cell also sees the list. Both are
/// filled even when the machine has no NVMe controller at all, because
/// `fs_main_thread` waits on this before it scans for partitions and would
/// otherwise never mount root on an AHCI-only machine.
pub static NVME_PROBE_DONE: Once<()> = Once::new();

/// The first namespace id a controller registers under in `block_io`. Ids
/// below this belong to AHCI (`0..1000`), USB storage (`1000..2000`) and the
/// ramdisk (`2000..3000`).
pub const NVME_DEVICE_ID_BASE: u64 = 3000;

/// Namespace ids per controller in the `block_io` id space. Also the largest
/// `nsid` this driver will register: a namespace numbered above it would
/// collide with the next controller's range. `devfs::block::device_name`
/// undoes this arithmetic to build `nvme<c>n<n>`.
pub const NVME_IDS_PER_CONTROLLER: u64 = 64;

pub fn init() {
    queue_spawn_kthread_named("nvme", nvme_driver_main as *const () as u64);
}

/// Map a completion's status field (SCT bits 11:9, SC bits 8:1 of
/// `NvmeQueue::drain`'s `status` argument) to a `BlockError`, per NVMe 2.0
/// 3.3.3 (Generic/Command-Specific/Media-and-Data-Integrity status types)
/// and the NVM Command Set's Read/Write status codes.
pub(crate) fn status_to_block_error(status: u16) -> Result<(), BlockError> {
    let sc = regs::status_code(status);
    let sct = regs::status_code_type(status);
    if sct == 0 && sc == 0 {
        return Ok(());
    }
    match (sct, sc) {
        (0, 0x02) | (0, 0x0B) | (0, 0x80) => Err(BlockError::InvalidArg),
        (0, 0x06) | (0, 0x82) => Err(BlockError::Io),
        (1, _) | (2, _) => Err(BlockError::Io),
        _ => {
            log!(
                "nvme: unmapped completion status SCT={:#x} SC={:#x}",
                sct,
                sc
            );
            Err(BlockError::Io)
        }
    }
}

/// How many times the watchdog re-scans the command slots trying to empty
/// them before giving up on this sweep. More than one because a submitter
/// can install a command after the scan, and a losing pass costs only
/// another walk of the slot array.
const FAIL_ALL_PASSES: usize = 4;

/// Retire a terminal op through the one reclaim sequence this driver has:
/// copy a bounced read back when it succeeded, return the bounce buffer and
/// the PRP list page to `dma()`, drop the cancel hookup, clear the command
/// slot, free the command id, and complete the caller's handle **last**.
///
/// The state CAS inside decides whether this call is the one that reclaims
/// or loses to a concurrent path, so both callers -- the IRQ dispatcher's
/// `complete_command` and the watchdog's fail-all -- can call it on the same
/// op without double-freeing a cid or handing the same `DmaBuffer` back
/// twice. `DmaBuffer` has no `Drop`: skipping this and letting the last
/// `Arc<NvmeOp>` fall strands its DMA memory for the life of the boot.
pub(crate) fn retire_op(queue: &queue::NvmeQueue, op: Arc<NvmeOp>, result: Result<(), BlockError>) {
    // `None` once the handle has already been completed elsewhere: a
    // cancelled op told its waiter `Cancelled` at cancel time and is only
    // here to have its reserved resources returned now the device is
    // finally done with them.
    let mut deliver = Some(result);
    match op.state.compare_exchange(
        cancel_op::OP_PENDING,
        cancel_op::OP_COMPLETED,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => {}
        Err(cancel_op::OP_CANCELLED) => {
            // `OP_CANCELLED` is not exclusive -- every later caller that
            // loses the CAS above observes it -- so the reclaim transition
            // picks exactly one winner.
            if op
                .state
                .compare_exchange(
                    cancel_op::OP_CANCELLED,
                    cancel_op::OP_RECLAIMED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                return;
            }
            deliver = None;
        }
        Err(_) => return, // already terminal via another completion path
    }

    let completion = op.completion.clone();
    let resources = op.take_resources();
    // Only a command the device actually completed has anything to copy
    // back. `allocate_sized_uninit` hands out a pooled page still carrying
    // whatever its frames last held, so on a failed read the bounce
    // contains another driver's bytes, and copying it would publish them
    // into the caller's buffer -- a page-cache page, once namespaces
    // register.
    if matches!(deliver, Some(Ok(())))
        && let cancel_op::Direction::Read = op.direction
        && let Some(bounce) = &resources.bounce
    {
        // SAFETY: the bounce buffer and the caller's buffer are both at
        // least `op.len` bytes (`build_transfer` sized the bounce
        // allocation to `len`, and the submit path validated the caller's
        // buffer against it before installing the op), and nothing else
        // touches either while the op is still installed in `cmd_slots`.
        unsafe {
            ptr::copy_nonoverlapping(bounce.as_ptr(), op.buffer.as_mut_ptr(), op.len);
        }
    }
    cancel_op::dealloc_resources(resources);
    if deliver.is_some()
        && let Some(t) = op.submitter.upgrade()
    {
        t.owned_ops_remove(Arc::as_ptr(&op) as *const ());
    }
    let cid = op.cid;
    queue.clear_cmd_slot(cid);
    queue.free_cid(cid);
    watchdog::inflight_dec();
    drop(op);
    if let Some(result) = deliver {
        completion.finish(result);
    }
}

impl NvmeController {
    /// IRQ-dispatcher-side completion for command id `cid` on queue `qid`.
    /// Mirrors AHCI's `complete_ncq_slot`: clone the op out from under
    /// `cmd_slots`' guard, drop the guard, then let [`retire_op`]'s own
    /// state CAS decide whether this call is the one that reclaims.
    fn complete_command(&self, qid: u16, cid: u16, status: u16) {
        let Some(queue) = self.queue_for(qid) else {
            log!("nvme: completion for unknown queue {}", qid);
            return;
        };
        if cid >= queue.cid_depth() as u16 {
            log!(
                "nvme: completion cid {} out of range for queue {}",
                cid,
                qid
            );
            return;
        }
        let cid = cid as u8;
        let Some(op) = queue.cmd_slot(cid) else {
            // A concurrent drain (the watchdog, or another dispatcher pass
            // racing the same CQ entry) already reclaimed this cid.
            return;
        };
        debug_assert_eq!(op.qid, qid, "nvme: op installed on the wrong queue");

        let result = status_to_block_error(status);
        if result.is_err() {
            stats::bump(&stats::COMMAND_ERRORS, 1);
        }
        retire_op(queue, op, result);
    }

    /// One watchdog pass over this controller's I/O queue.
    ///
    /// Drains the completion queue first: anything found there is a lost
    /// interrupt, not a hung device, and recovering it costs a tick of
    /// latency where a reset would have failed live I/O for nothing. Only
    /// what is still outstanding *after* that drain, and older than
    /// `timeout`, counts as hung -- as does `CSTS.CFS`, which the
    /// controller sets when it has failed outright.
    pub fn watchdog_sweep(&self, timeout: Duration) {
        let Some(queue) = self.io_queue() else {
            return;
        };
        let mut recovered = 0u64;
        queue.drain(|cid, status, _dw0| {
            recovered += 1;
            self.complete_command(admin::IO_QID, cid, status);
        });
        if recovered != 0 {
            watchdog::WATCHDOG_COMPLETIONS.fetch_add(recovered, Ordering::Relaxed);
        }

        let now = crate::timer::Instant::now().as_nanos();
        let timeout_ns = timeout.as_nanos() as u64;
        let outstanding = queue.outstanding_ops();
        let hung = outstanding
            .iter()
            .filter(|op| {
                op.state.load(Ordering::Acquire) == cancel_op::OP_PENDING && {
                    let issued = op.issue_time.load(Ordering::Relaxed);
                    issued != 0 && now.saturating_sub(issued) >= timeout_ns
                }
            })
            .count() as u64;
        let fatal = self.csts() & regs::CSTS_CFS != 0;
        if hung == 0 && !fatal {
            return;
        }

        // One reset at a time, and none while an earlier one is still
        // rebuilding the queues.
        if self.begin_restart().is_err() {
            return;
        }
        watchdog::WATCHDOG_FIRINGS.fetch_add(hung, Ordering::Relaxed);
        log!(
            "nvme: watchdog firing, {} command(s) past {} ms{}",
            hung,
            timeout.as_millis(),
            if fatal { ", CSTS.CFS set" } else { "" }
        );

        // Every outstanding op is failed through the same reclaim sequence
        // a completion uses, before the reset: clearing the slots any other
        // way would strand each op's bounce buffer and PRP list page, since
        // `DmaBuffer` has no `Drop`.
        //
        // Re-scanned rather than reusing the snapshot above, and in a loop:
        // nothing stops a submitter from installing a command between the
        // scan and the reset, and that command is killed by the reset too,
        // so it has to be failed here or it waits forever for a completion
        // the controller will never post. A slot can also be momentarily
        // occupied by an op the dispatcher is already retiring, which the
        // next pass sees gone.
        drop(outstanding);
        for _ in 0..FAIL_ALL_PASSES {
            for op in queue.outstanding_ops() {
                retire_op(queue, op, Err(BlockError::Io));
            }
            if queue.cmd_slots_empty() {
                break;
            }
        }
        if !queue.cmd_slots_empty() {
            // Resetting now would clear the slots and strand their DMA
            // memory. Skipping costs a tick: the next sweep tries again,
            // and the commands that are still installed are still hung.
            log!("nvme: watchdog could not quiesce the queue, deferring the reset");
            self.end_restart();
            return;
        }

        match self.reset_controller() {
            Ok(()) => {
                watchdog::WATCHDOG_RESETS.fetch_add(1, Ordering::Relaxed);
                log!("nvme: controller reset complete");
            }
            Err(e) => log!("nvme: controller reset failed: {:?}", e),
        }
        self.end_restart();
    }
}

/// Set `CC.SHN` on every controller and wait for `CSTS.SHST` to report the
/// shutdown complete, so a controller with a volatile write cache commits it
/// (NVMe 2.0 3.6.2). Called from `power::quiesce` after the filesystems are
/// synced, and like the rest of that path it prints rather than logs: the
/// ring buffer is not read again after this point.
pub fn shutdown_all() {
    let Some(controllers) = NVME_CONTROLLERS.get() else {
        return;
    };
    for (index, controller) in controllers.iter().enumerate() {
        match controller.shutdown() {
            Ok(()) => println!("nvme{index}: shutdown complete"),
            Err(e) => println!("nvme{index}: shutdown did not complete: {e:?}"),
        }
    }
}

pub extern "C" fn nvme_driver_main() -> ! {
    let thread = current_thread().unwrap();
    thread.set_priority(IO_PRIORITY);
    crate::interrupts::io::NVME_DRIVER_THREAD_ID
        .call_once(|| current_thread_weak().unwrap_or_default());

    let devices: Vec<PciDevice> = pci_manager().read().get_devices().to_vec();

    let mut controllers: Vec<Arc<NvmeController>> = Vec::new();
    for device in devices {
        if device.header.class_code != 0x01
            || device.header.subclass != 0x08
            || device.header.prog_if != 0x02
        {
            continue;
        }
        match NvmeController::new(device) {
            Ok(controller) => controllers.push(Arc::new(controller)),
            Err(e) => log!("nvme: failed to initialize controller: {:?}", e),
        }
    }

    let mut namespaces: Vec<Arc<NvmeNamespace>> = Vec::new();

    for (controller_index, controller) in controllers.iter().enumerate() {
        let ident = match controller.identify_controller() {
            Ok(ident) => ident,
            Err(e) => {
                log!(
                    "nvme{}: identify controller failed: {:?}",
                    controller_index,
                    e
                );
                continue;
            }
        };
        let mdts_bytes = max_transfer_bytes(ident.mdts, regs::cap_mpsmin(controller.cap()));
        let vwc = ident.write_cache_present();
        let model = ident.model_trimmed();
        let serial = ident.serial_trimmed();

        if let Err(e) = controller.setup_io_queue() {
            log!(
                "nvme{}: I/O queue pair setup failed: {:?}",
                controller_index,
                e
            );
            continue;
        }

        let nsids = match controller.active_namespace_ids() {
            Ok(ids) => ids,
            Err(e) => {
                log!(
                    "nvme{}: active namespace list failed: {:?}",
                    controller_index,
                    e
                );
                continue;
            }
        };

        for nsid in nsids {
            let ns = match controller.identify_namespace(nsid) {
                Ok(ns) => ns,
                Err(e) => {
                    log!(
                        "nvme{}n{}: identify namespace failed: {:?}",
                        controller_index,
                        nsid,
                        e
                    );
                    continue;
                }
            };
            log!(
                "nvme{}n{}: {} sn={} {} LBAs of {} B, mdts={}, vwc={}",
                controller_index,
                nsid,
                model,
                serial,
                ns.nsze,
                ns.lba_size(),
                mdts_bytes,
                vwc
            );

            // Everything above `block_io` counts in 512-byte sectors and has
            // nowhere to put per-block metadata, so a namespace this driver
            // cannot present as a plain 512-byte disk is refused by name
            // rather than registered and misread.
            let refusal = if ns.lba_size() != 512 {
                Some("logical block size is not 512 B")
            } else if ns.metadata_size() != 0 {
                Some("namespace carries per-block metadata")
            } else if nsid == 0 || u64::from(nsid) > NVME_IDS_PER_CONTROLLER {
                Some("namespace id is outside this driver's id range")
            } else {
                None
            };
            if let Some(why) = refusal {
                log!("nvme{controller_index}n{nsid}: refused, {why}");
                continue;
            }

            let device_id = NVME_DEVICE_ID_BASE
                + controller_index as u64 * NVME_IDS_PER_CONTROLLER
                + u64::from(nsid - 1);
            let namespace = Arc::new(NvmeNamespace::new(
                Arc::clone(controller),
                nsid,
                ns.nsze,
                device_id,
                mdts_bytes,
                vwc,
            ));
            block_io::register(
                namespace.device_id(),
                Arc::clone(&namespace) as Arc<dyn AsyncBlockDevice>,
            );
            log!(
                "nvme{controller_index}n{nsid}: registered as block device {}",
                namespace.device_id()
            );
            namespaces.push(namespace);
        }
    }

    // Published before the barrier is signalled, so a thread released by
    // `wait_probe_complete` also sees the lists and the registrations.
    let controllers = NVME_CONTROLLERS.call_once(|| controllers);
    NVME_NAMESPACES.call_once(|| namespaces);
    NVME_PROBE_DONE.call_once(|| ());

    // Only worth a thread once there is something to sweep; a machine with
    // no controller would otherwise wake once a second forever.
    if !controllers.is_empty() {
        queue_spawn_kthread_named(
            "nvme_watchdog",
            watchdog::watchdog_entry as *const () as u64,
        );
    }

    if NVME_PROBE_READ.load(Ordering::Relaxed) {
        match api::namespaces().first() {
            Some(ns) => {
                let args = Box::new(ProbeReadArgs {
                    namespace: Arc::clone(ns),
                });
                queue_spawn_kthread_named_arg(
                    "nvme_probe_read",
                    nvme_probe_read_thread as *const () as u64,
                    Box::into_raw(args).cast(),
                );
            }
            None => log!("nvme: probe read requested but no supported namespace was found"),
        }
    }

    // Interrupt dispatch loop. The admin and I/O CQs of every controller
    // share the single vector `configure_interrupt` bound (`IV = 0` on
    // both), so a wake says only that some queue somewhere has a
    // completion; this loop drains every controller's I/O queue rather than
    // routing by vector.
    //
    // The admin queue is deliberately not drained here. Admin commands are
    // polled by their issuer (`admin_command_polled` runs its own `drain`),
    // and a completion consumed by this thread is one that poll never sees:
    // the command times out instead. That is unreachable at bring-up, where
    // this loop has not started yet, but not during a controller reset,
    // which re-runs Set Features and Create I/O Queue from the watchdog
    // thread while this one is live.
    //
    // The predicate is the interrupt counter rather than the queues
    // themselves. A park predicate runs with interrupts off after the CPU has
    // pivoted onto the transition stack, so it must not take a lock any other
    // thread can hold -- and the watchdog drains the I/O completion queue
    // under `NvmeQueue.cq` from its own thread. Reading `seen` *before* the
    // drain is what makes it lossless: a completion posted after the read is
    // either picked up by this pass anyway, costing one spurious pass next
    // time round, or lands after it and leaves the counter ahead, so the next
    // park returns at once.
    let mut seen = NVME_IRQS_FIRED.load(Ordering::Acquire);
    loop {
        thread_park_while(|| NVME_IRQS_FIRED.load(Ordering::Acquire) == seen);
        seen = NVME_IRQS_FIRED.load(Ordering::Acquire);
        watchdog::DISPATCHER_PASSES.fetch_add(1, Ordering::Relaxed);

        for controller in controllers {
            if let Some(io_queue) = controller.io_queue() {
                io_queue.drain(|cid, status, _dw0| {
                    controller.complete_command(IO_QID, cid, status);
                });
            }
        }
    }
}

struct ProbeReadArgs {
    namespace: Arc<NvmeNamespace>,
}

/// `nvme_probe_read` cmdline gate: reads LBA 0 of the first supported
/// namespace found and logs whether it carries the protective-MBR
/// signature (`0x55 0xAA` at byte 510), the observable this phase's read
/// path is verified against before anything in the kernel depends on it.
extern "C" fn nvme_probe_read_thread(arg: *mut ProbeReadArgs) -> ! {
    let args = *unsafe { Box::from_raw(arg) };
    let ns = args.namespace;

    let read = |lba: u64, sectors: u32| -> Option<Arc<Vec<u8>>> {
        let buf = Arc::new(alloc::vec![0u8; sectors as usize * 512]);
        match ns.submit_read(lba, sectors, BlockBuffer::owned_vec(buf.clone())) {
            Ok(handle) => match handle.wait() {
                Ok(()) => Some(buf),
                Err(e) => {
                    log!("nvme: probe read lba {lba} wait failed: {e:?}");
                    None
                }
            },
            Err(e) => {
                log!("nvme: probe read lba {lba} submit failed: {e:?}");
                None
            }
        }
    };

    // One sector: PRP1 alone, and the only case with content this driver can
    // check against a known value.
    if let Some(buf) = read(0, 1) {
        let signature_ok = buf[510] == 0x55 && buf[511] == 0xAA;
        log!(
            "nvme: probe read LBA0 bytes[510..512]={:#04x} {:#04x} ({})",
            buf[510],
            buf[511],
            if signature_ok {
                "protective MBR signature found"
            } else {
                "signature not found"
            }
        );
    }

    // A PRP list only gates the translation if the buffer it describes is
    // physically discontiguous. A run of frames that the naive `first page +
    // n * 4096` derivation happens to get right proves nothing, and the
    // kernel heap hands out such runs readily at boot, so the buffer is
    // built discontiguous rather than allocated and hoped over. Growing it
    // is the fallback for the one case construction cannot rule out: a
    // concurrent allocation taking one of the comb's holes.
    const CANDIDATE_PAGES: [u64; 4] = [4, 16, 64, 256];
    let max_pages = (ns.max_transfer_bytes() / 4096).min(PRP_LIST_ENTRIES) as u64;
    let mut gate = None;
    for pages in CANDIDATE_PAGES {
        if pages > max_pages {
            break;
        }
        let bytes = (pages * 4096) as usize;
        let Some(buf) = discontiguous_buffer(pages) else {
            log!("nvme: probe could not build a {pages}-page discontiguous buffer");
            break;
        };
        let before = stats::PRP_PAGES_DISCONTIGUOUS.load(Ordering::Relaxed);
        // SAFETY: the buffer is mapped writable for `bytes` and this thread
        // waits on the handle below before it can reach a kill point.
        let block_buf = unsafe { BlockBuffer::reaped_by_submitter(buf.as_mut_ptr::<u8>(), bytes) };
        let read_ok = match ns.submit_read(0, (pages as u32) * 8, block_buf) {
            Ok(handle) => match handle.wait() {
                Ok(()) => true,
                Err(e) => {
                    log!("nvme: probe {pages}-page read wait failed: {e:?}");
                    false
                }
            },
            Err(e) => {
                log!("nvme: probe {pages}-page read submit failed: {e:?}");
                false
            }
        };
        if !read_ok {
            release_probe_buffer(buf, pages);
            break;
        }
        let scattered = stats::PRP_PAGES_DISCONTIGUOUS.load(Ordering::Relaxed) - before;
        if scattered > 0 {
            gate = Some((pages, buf, bytes, scattered));
            break;
        }
        release_probe_buffer(buf, pages);
        log!("nvme: probe {pages}-page buffer came back physically contiguous, growing it");
    }

    // Nothing on the disk has a known value at that offset, so the check is
    // self-consistency: the same bytes read one page at a time must match
    // the bytes read in one command. A PRP list that addresses the wrong
    // frames reads unrelated memory and fails here while the single-sector
    // case above still passes.
    match gate {
        None => log!(
            "nvme: PRP GATE NOT DISCRIMINATING -- no candidate buffer up to {} pages was \
             physically discontiguous, so the multi-page read below would pass with the \
             page addressing broken",
            CANDIDATE_PAGES[CANDIDATE_PAGES.len() - 1].min(max_pages)
        ),
        Some((pages, buf, bytes, scattered)) => {
            // SAFETY: `discontiguous_buffer` mapped `bytes` writable at
            // `buf` and the read above has completed, so the range is
            // initialised and nothing else refers to it.
            let whole = unsafe { core::slice::from_raw_parts(buf.as_ptr::<u8>(), bytes) };
            let mut per_page = Vec::new();
            for page in 0..pages {
                match read(page * 8, 8) {
                    Some(b) => per_page.push(b),
                    None => break,
                }
            }
            if per_page.len() != pages as usize {
                log!("nvme: probe multi-page read did not complete");
            } else {
                let joined: Vec<u8> = per_page.iter().flat_map(|b| b.iter().copied()).collect();
                match whole.iter().zip(joined.iter()).position(|(a, b)| a != b) {
                    None => log!(
                        "nvme: PRP gate discriminating: {pages} pages via PRP list, {scattered} \
                         of them not where naive addressing would have looked, matches {pages} \
                         single-page reads"
                    ),
                    Some(off) => log!(
                        "nvme: PRP LIST MISMATCH at byte {off} (page {}): one-shot {:#04x}, per-page {:#04x}",
                        off / 4096,
                        whole[off],
                        joined[off]
                    ),
                }
            }
            release_probe_buffer(buf, pages);
        }
    }

    thread_exit(0);
}

/// A `pages`-page kernel buffer that is physically discontiguous by
/// construction, so a PRP list describing it addresses frames the naive
/// "first frame plus the page index" derivation gets wrong.
///
/// [`BitmapFrameAllocator`](crate::memory::frame_allocator) scans forward
/// from its free hint and moves that hint back to any frame freed below it.
/// Mapping a comb of `2 * pages` frames and unmapping every other one
/// therefore leaves an alternating free/busy run that the next `pages`
/// allocations are served from in ascending order -- comb frame 0, 2, 4 --
/// so every page of the returned buffer sits at least two frames past its
/// predecessor. Only a concurrent allocator taking one of those holes can
/// spoil it, which is why the caller still checks the counter rather than
/// assuming.
///
/// The comb's virtual range is never read or written again, so the stale
/// kernel TLB entries the unmaps leave on other CPUs are unreachable and no
/// shootdown is issued for them.
fn discontiguous_buffer(pages: u64) -> Option<VirtAddr> {
    const PAGE: u64 = 4096;
    let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;

    let comb_bytes = pages * 2 * PAGE;
    let comb = vmalloc(comb_bytes)?;
    let buf = vmalloc(pages * PAGE)?;

    let mut mapper = memory_mapper();
    mapper.map_memory(comb, comb_bytes, flags).ok()?;
    for i in (0..pages * 2).step_by(2) {
        mapper.unmap_memory(comb + i * PAGE, PAGE).ok()?;
    }
    mapper.map_memory(buf, pages * PAGE, flags).ok()?;
    for i in (1..pages * 2).step_by(2) {
        mapper.unmap_memory(comb + i * PAGE, PAGE).ok()?;
    }
    drop(mapper);
    vfree(comb, comb_bytes);

    Some(buf)
}

/// Returns a [`discontiguous_buffer`] to the frame allocator and the kernel
/// address space.
fn release_probe_buffer(buf: VirtAddr, pages: u64) {
    let bytes = pages * 4096;
    let _ = memory_mapper().unmap_memory(buf, bytes);
    vfree(buf, bytes);
}
