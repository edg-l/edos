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
};

use spin::Once;
use thiserror::Error;

use crate::{
    drivers::{
        block_io::{self, AsyncBlockDevice, BlockBuffer, BlockError},
        nvme::{
            admin::{IO_QID, NvmeController},
            identify::max_transfer_bytes,
            namespace::NvmeNamespace,
        },
        pci::{pci_manager, structures::PciDevice},
    },
    log,
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

impl NvmeController {
    /// IRQ-dispatcher-side completion for command id `cid` on queue `qid`.
    /// Mirrors AHCI's `complete_ncq_slot`: clone the op out from under
    /// `cmd_slots`' guard, drop the guard, then let the op's own state CAS
    /// decide whether this call is the one that reclaims the cid, the
    /// bounce buffer, the PRP list page and the handle, or loses to a
    /// concurrent cancel or a second drain of the same entry.
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

        let handle = Arc::clone(&op.handle);
        let mut pending_result = None;
        match op.state.compare_exchange(
            cancel_op::OP_PENDING,
            cancel_op::OP_COMPLETED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                let result = status_to_block_error(status);
                let resources = op.take_resources();
                // Only a command the device actually completed has anything
                // to copy back. `allocate_sized_uninit` hands out a pooled
                // page still carrying whatever its frames last held, so on a
                // failed read the bounce contains another driver's bytes,
                // and copying it would publish them into the caller's buffer
                // -- a page-cache page, once namespaces register.
                if result.is_ok()
                    && let cancel_op::Direction::Read = op.direction
                    && let Some(bounce) = &resources.bounce
                {
                    // SAFETY: the bounce buffer and the caller's buffer are
                    // both at least `op.len` bytes (`submit_read` sized the
                    // bounce allocation to `len` and validated the caller's
                    // buffer against it before installing the op), and
                    // nothing else touches either while the op is still
                    // installed in `cmd_slots`.
                    unsafe {
                        ptr::copy_nonoverlapping(bounce.as_ptr(), op.buffer.as_mut_ptr(), op.len);
                    }
                }
                cancel_op::dealloc_resources(resources);
                pending_result = Some(result);
                if let Some(t) = op.submitter.upgrade() {
                    t.owned_ops_remove(Arc::as_ptr(&op) as *const ());
                }
            }
            Err(cancel_op::OP_CANCELLED) => {
                // The submitter cancelled while this command was still
                // issued to the device; its handle is already completed
                // with `Cancelled`. The device has now actually finished,
                // so this is where the cid and the op's DMA memory are
                // reclaimed -- not at cancel time, when the device still
                // considered the command outstanding.
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
                    // Another completion path already won the reclaim.
                    return;
                }
                cancel_op::dealloc_resources(op.take_resources());
            }
            Err(_) => return, // already terminal via another completion path
        }
        queue.clear_cmd_slot(cid);
        queue.free_cid(cid);
        drop(op);
        if let Some(result) = pending_result {
            handle.complete(result);
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
    // `wait_probe_complete` also sees the list and the registrations.
    NVME_NAMESPACES.call_once(|| namespaces);
    NVME_PROBE_DONE.call_once(|| ());

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
    // both), so any wake drains every queue of every controller rather than
    // routing by vector.
    loop {
        thread_park_while(|| {
            !controllers.iter().any(|c| {
                c.admin_queue().has_pending() || c.io_queue().is_some_and(|q| q.has_pending())
            })
        });

        for controller in &controllers {
            controller.admin_queue().drain(|cid, status, _dw0| {
                controller.complete_command(0, cid, status);
            });
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

    // Thirty-two sectors span four pages, so this is the first read to build
    // a PRP list rather than PRP1/PRP2 alone. Nothing on the disk has a
    // known value at that offset, so the check is self-consistency: the same
    // bytes read one page at a time must match the bytes read in one
    // command. A PRP list that addresses the wrong frames -- deriving later
    // pages by adding 4096 to the first, when the kernel heap is mapped a
    // frame at a time -- reads unrelated memory and fails here while the
    // single-sector case above still passes.
    const PAGES: u64 = 4;
    let whole = read(0, (PAGES as u32) * 8);
    let mut per_page = Vec::new();
    for page in 0..PAGES {
        match read(page * 8, 8) {
            Some(b) => per_page.push(b),
            None => break,
        }
    }
    match whole {
        Some(whole) if per_page.len() == PAGES as usize => {
            let joined: Vec<u8> = per_page.iter().flat_map(|b| b.iter().copied()).collect();
            let mismatch = whole.iter().zip(joined.iter()).position(|(a, b)| a != b);
            match mismatch {
                None => log!(
                    "nvme: probe read {} pages via PRP list matches {} single-page reads",
                    PAGES,
                    PAGES
                ),
                Some(off) => log!(
                    "nvme: PRP LIST MISMATCH at byte {off} (page {}): one-shot {:#04x}, per-page {:#04x}",
                    off / 4096,
                    whole[off],
                    joined[off]
                ),
            }
        }
        _ => log!("nvme: probe multi-page read did not complete"),
    }

    thread_exit(0);
}
