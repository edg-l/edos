//! Controller bring-up (NVMe 2.0 3.5.1) and polled admin commands.

use core::{
    ptr,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use alloc::vec::Vec;
use spin::Once;
use x86_64::structures::paging::{PageTableFlags, mapper::MapToError};

use crate::{
    debug::lock_order::RANK_NVME_ADMIN,
    drivers::{
        dma::{DmaBuffer, dma},
        msi,
        nvme::{
            NvmeError,
            identify::{IdentifyController, IdentifyNamespace},
            queue::NvmeQueue,
            regs::{self, NvmeRegs, SubmissionQueueEntry},
        },
        pci::{
            config::{pci_read_u16, pci_write_u16, read_bar_phys},
            structures::PciDevice,
        },
    },
    interrupts::InterruptIndex,
    log,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    ranked_lock,
    thread::{mutex::BlockingMutex, scheduler::thread_sleep},
    timer::Instant,
};

/// PCI Command register offset and the bits this driver sets explicitly
/// rather than trusting firmware: Memory Space Enable (bit 1) and Bus
/// Master Enable (bit 2).
const PCI_COMMAND_OFFSET: u8 = 0x04;
const PCI_COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const PCI_COMMAND_BUS_MASTER: u16 = 1 << 2;

/// Bytes of `BAR0` mapped: the fixed register block plus room for a handful
/// of doorbells at the default stride.
const BAR0_MAP_SIZE: u64 = 0x2000;

const ADMIN_SQ_ENTRIES: u16 = 32;
const ADMIN_CQ_ENTRIES: u16 = 32;
/// Admin commands are always issued one at a time under `admin`, but the
/// cid space is capped well below the ring size so exhaustion is visible if
/// that ever stops being true.
const ADMIN_CID_DEPTH: u8 = 8;

/// The queue id of this driver's one I/O queue pair (Architecture Decision
/// "one I/O queue pair, one MSI-X vector, no polled I/O stage").
pub const IO_QID: u16 = 1;
const IO_SQ_ENTRIES: u16 = 128;
const IO_CQ_ENTRIES: u16 = 128;
/// Command ids come from a single 64-bit bitmap, so at most 64 commands are
/// ever outstanding against the 128-entry ring: the SQ can never fill.
const IO_CID_DEPTH: u8 = 64;

/// The `CC` value this driver runs a controller at: enabled, the NVM
/// command set, a 4 KiB host page size, 64-byte SQ entries and 16-byte CQ
/// entries. Written at bring-up and again by every reset, so the two cannot
/// drift.
const CC_ENABLED: u32 = regs::CC_EN
    | (0 << regs::CC_CSS_SHIFT)
    | (0 << regs::CC_MPS_SHIFT)
    | (6 << regs::CC_IOSQES_SHIFT)
    | (4 << regs::CC_IOCQES_SHIFT);

/// `AQA`: the admin queue sizes, both 0's-based.
const ADMIN_AQA: u32 = ((ADMIN_CQ_ENTRIES as u32 - 1) << 16) | (ADMIN_SQ_ENTRIES as u32 - 1);

/// Set Features (09h) Feature Identifier for Number of Queues (NVM Command
/// Set 5.21.1.7).
const FEATURE_NUMBER_OF_QUEUES: u32 = 0x07;

pub struct NvmeController {
    #[expect(dead_code, reason = "read once MSI-X/MSI interrupt configuration runs")]
    pub pci_device: PciDevice,
    regs: *mut NvmeRegs,
    cap: u64,
    /// Serializes admin command issue and controller state transitions
    /// (init, queue create/delete, reset) so at most one is ever in flight.
    admin: BlockingMutex<()>,
    admin_queue: NvmeQueue,
    /// This driver's one I/O queue pair, created by `setup_io_queue` once
    /// the controller is enabled and its interrupt is bound. `None` until
    /// then, and while it stays `None` this controller has no working
    /// namespace: nothing issues an NVM command against a queue that does
    /// not exist yet.
    io_queue: Once<NvmeQueue>,
    /// Set while the watchdog is failing this controller's outstanding
    /// commands and rebuilding its queues. One reset at a time, and none
    /// nested inside another.
    restarting: AtomicBool,
}

// SAFETY: `regs` is a stable MMIO mapping read and written only through
// volatile accesses; every other field is already `Send + Sync`.
unsafe impl Send for NvmeController {}
unsafe impl Sync for NvmeController {}

impl NvmeController {
    pub fn new(pci_device: PciDevice) -> Result<Self, NvmeError> {
        if pci_device.header.class_code != 0x01
            || pci_device.header.subclass != 0x08
            || pci_device.header.prog_if != 0x02
        {
            return Err(NvmeError::InvalidDevice);
        }

        log!(
            "nvme: probing {:02x}:{:02x}.{}",
            pci_device.address.bus,
            pci_device.address.device,
            pci_device.address.function
        );

        // AHCI relies on firmware to have set these; NVMe does not.
        let mut command = pci_read_u16(pci_device.address, PCI_COMMAND_OFFSET);
        command |= PCI_COMMAND_MEMORY_SPACE | PCI_COMMAND_BUS_MASTER;
        pci_write_u16(pci_device.address, PCI_COMMAND_OFFSET, command);

        let bar0_phys = read_bar_phys(pci_device.address, 0);
        let bar0_virt = get_virt_addr_from_phys_offset(bar0_phys);
        {
            let mut mapper = memory_mapper();
            if let Err(e) = mapper.map_address_range(
                bar0_virt,
                bar0_phys,
                BAR0_MAP_SIZE as usize,
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::GLOBAL,
            ) {
                match e {
                    MapToError::PageAlreadyMapped(_) => {}
                    _ => return Err(NvmeError::InvalidDevice),
                }
            }
        }

        let regs = bar0_virt.as_mut_ptr::<NvmeRegs>();
        let cap = unsafe { ptr::read_volatile(&raw const (*regs).cap) };

        if regs::cap_mpsmin(cap) != 0 {
            log!(
                "nvme: MPSMIN={} unsupported (only a 4 KiB host page size is implemented)",
                regs::cap_mpsmin(cap)
            );
            return Err(NvmeError::Unsupported);
        }
        if regs::cap_css(cap) & 1 == 0 {
            log!(
                "nvme: CAP.CSS={:#x} does not offer the NVM command set",
                regs::cap_css(cap)
            );
            return Err(NvmeError::Unsupported);
        }

        // CAP.DSTRD scales the doorbell stride, and the mapping is a fixed
        // BAR0_MAP_SIZE. A large stride pushes even the admin queue's
        // doorbells past the mapped window, where a write lands on
        // whatever else happens to be there. QEMU always reports 0, so
        // nothing local exercises this.
        let dstrd = regs::cap_dstrd(cap);
        let highest_doorbell = regs::cq_head_doorbell_offset(1, dstrd);
        if highest_doorbell >= BAR0_MAP_SIZE as usize {
            log!(
                "nvme: DSTRD={} needs a doorbell at {:#x}, past the {:#x} mapping; unsupported",
                dstrd,
                highest_doorbell,
                BAR0_MAP_SIZE
            );
            return Err(NvmeError::Unsupported);
        }

        // MQES is 0's-based, so it is the largest queue this controller
        // accepts minus one. QEMU reports 2047; a controller reporting
        // fewer entries than the admin queue wants gets refused rather
        // than programmed with a size it never agreed to.
        let mqes = regs::cap_mqes(cap);
        if ADMIN_SQ_ENTRIES - 1 > mqes || ADMIN_CQ_ENTRIES - 1 > mqes {
            log!(
                "nvme: MQES={} is below the {}-entry admin queue; unsupported",
                mqes,
                ADMIN_SQ_ENTRIES
            );
            return Err(NvmeError::Unsupported);
        }

        // CAP.TO is in 500 ms units and may legally read 0, which would
        // expire the deadline before a completion could arrive.
        let timeout =
            Duration::from_millis(regs::cap_to(cap) as u64 * 500).max(Duration::from_secs(1));

        // Clear CC.EN and wait for CSTS.RDY to drop before reprogramming the
        // admin queue registers (NVMe 2.0 3.5.1 requires the controller be
        // disabled while ASQ/ACQ/AQA change).
        unsafe { ptr::write_volatile(&raw mut (*regs).cc, 0) };
        wait_csts(regs, regs::CSTS_RDY, false, timeout)?;

        let admin_queue = NvmeQueue::new(
            0,
            ADMIN_SQ_ENTRIES,
            ADMIN_CQ_ENTRIES,
            ADMIN_CID_DEPTH,
            dstrd,
            bar0_virt,
        )?;

        unsafe {
            ptr::write_volatile(&raw mut (*regs).aqa, ADMIN_AQA);
            ptr::write_volatile(&raw mut (*regs).asq, admin_queue.sq_phys_addr().as_u64());
            ptr::write_volatile(&raw mut (*regs).acq, admin_queue.cq_phys_addr().as_u64());
        }

        unsafe { ptr::write_volatile(&raw mut (*regs).cc, CC_ENABLED) };
        if let Err(e) = wait_csts(regs, regs::CSTS_RDY, true, timeout) {
            admin_queue.dealloc_all();
            return Err(e);
        }

        log!(
            "nvme: controller enabled, MQES={}, TO={}ms, DSTRD={}",
            regs::cap_mqes(cap),
            timeout.as_millis(),
            dstrd
        );

        if let Err(e) = Self::configure_interrupt(&pci_device) {
            admin_queue.dealloc_all();
            return Err(e);
        }

        Ok(Self {
            pci_device,
            regs,
            cap,
            admin: BlockingMutex::new(()),
            admin_queue,
            io_queue: Once::new(),
            restarting: AtomicBool::new(false),
        })
    }

    /// Bind this controller's completion queues to one interrupt: MSI-X
    /// table entry 0, falling back to a single MSI message (Architecture
    /// Decision "one I/O queue pair, one MSI-X vector"). The admin and I/O
    /// CQs are both later created with `IV = 0`, so this one vector is all
    /// either ever needs.
    ///
    /// There is deliberately no legacy INTx fallback. An NVMe pin-based
    /// interrupt stays level-asserted until the CQ head doorbell is written
    /// (NVMe base spec 2.0 §3.5.1), and this driver writes that doorbell
    /// from the dispatcher thread, not from the handler -- so the IOAPIC
    /// would re-deliver until the dispatcher is scheduled. Masking through
    /// `INTMS`/`INTMC` around the drain would fix that, but nothing this
    /// driver runs on offers a controller without MSI or MSI-X, so the
    /// untestable path is refused by name instead of half-implemented.
    fn configure_interrupt(pci_device: &PciDevice) -> Result<(), NvmeError> {
        if msi::enable_msix_for_device(pci_device, InterruptIndex::Nvme.as_u8(), 0).is_ok() {
            log!(
                "nvme: MSI-X bound on {:02x}:{:02x}.{}",
                pci_device.address.bus,
                pci_device.address.device,
                pci_device.address.function
            );
            return Ok(());
        }
        if msi::enable_msi_for_device(pci_device, InterruptIndex::Nvme.as_u8()).is_ok() {
            log!(
                "nvme: MSI bound on {:02x}:{:02x}.{}",
                pci_device.address.bus,
                pci_device.address.device,
                pci_device.address.function
            );
            return Ok(());
        }
        log!("nvme: controller offers neither MSI-X nor MSI; unsupported");
        Err(NvmeError::Unsupported)
    }

    pub fn cap(&self) -> u64 {
        self.cap
    }

    /// `CSTS`, read fresh. The watchdog reads `CSTS.CFS` through this.
    pub fn csts(&self) -> u32 {
        unsafe { ptr::read_volatile(&raw const (*self.regs).csts) }
    }

    /// How long this controller is allowed for a state transition.
    /// `CAP.TO` is in 500 ms units and may legally read 0, which would
    /// expire a deadline before the transition could possibly finish.
    fn controller_timeout(&self) -> Duration {
        Duration::from_millis(regs::cap_to(self.cap) as u64 * 500).max(Duration::from_secs(1))
    }

    /// Claim the right to reset this controller, or `Err(())` if a reset is
    /// already under way.
    pub fn begin_restart(&self) -> Result<(), ()> {
        self.restarting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| ())
    }

    pub fn end_restart(&self) {
        self.restarting.store(false, Ordering::Release);
    }

    /// Take the controller through a full reset (NVMe 2.0 3.5.1): disable,
    /// wait for `CSTS.RDY` to drop, fail every outstanding command,
    /// reinitialise both queue pairs in host memory, re-enable, then
    /// renegotiate the queue count and recreate the I/O queue pair on the
    /// device -- the controller forgot both when it was disabled.
    ///
    /// `fail_all` is the caller's fail-every-outstanding-command pass, and
    /// it is a parameter rather than something the caller runs first
    /// **because the order is the correctness argument**. Failing a command
    /// releases its buffer, and that buffer is usually not a `dma()` page:
    /// `build_transfer` describes the caller's own pages whenever it can, so
    /// the device writes straight into a page-cache frame or a kernel-heap
    /// `Vec`. Release one while `CSTS.RDY` is still set and the controller
    /// may still complete the command it was given, into memory the
    /// allocator has since handed to somebody else -- a use-after-free whose
    /// writer is the device, which no CPU-side check can see. Clearing
    /// `CC.EN` and waiting for `CSTS.RDY` to drop is what makes the
    /// controller stop touching host memory, so it happens first and
    /// `fail_all` does not run at all if it fails.
    ///
    /// `NvmeQueue::reset_state` fails whatever a submitter installed in the
    /// window since, so no command's bounce buffer or PRP list page is
    /// stranded either way.
    pub fn reset_controller(&self, fail_all: impl FnOnce()) -> Result<(), NvmeError> {
        {
            // Held only across the register-level transition: the admin
            // commands below take it themselves, and `BlockingMutex` is not
            // reentrant.
            let _guard = ranked_lock!(RANK_NVME_ADMIN, "NvmeController.reset", self.admin);
            let timeout = self.controller_timeout();
            unsafe { ptr::write_volatile(&raw mut (*self.regs).cc, 0) };
            wait_csts(self.regs, regs::CSTS_RDY, false, timeout)?;

            fail_all();

            self.admin_queue.reset_state();
            if let Some(queue) = self.io_queue() {
                queue.reset_state();
            }

            unsafe {
                ptr::write_volatile(&raw mut (*self.regs).aqa, ADMIN_AQA);
                ptr::write_volatile(
                    &raw mut (*self.regs).asq,
                    self.admin_queue.sq_phys_addr().as_u64(),
                );
                ptr::write_volatile(
                    &raw mut (*self.regs).acq,
                    self.admin_queue.cq_phys_addr().as_u64(),
                );
                ptr::write_volatile(&raw mut (*self.regs).cc, CC_ENABLED);
            }
            wait_csts(self.regs, regs::CSTS_RDY, true, timeout)?;
        }

        self.set_num_queues()?;
        if let Some(queue) = self.io_queue() {
            self.create_io_cq(queue)?;
            self.create_io_sq(queue)?;
        }
        Ok(())
    }

    /// Normal shutdown (NVMe 2.0 3.6.2): set `CC.SHN` to `01b` and wait for
    /// `CSTS.SHST` to report `10b`, which is what commits a volatile write
    /// cache. Skipping it risks losing data the controller acknowledged but
    /// had not written down.
    pub fn shutdown(&self) -> Result<(), NvmeError> {
        let _guard = ranked_lock!(RANK_NVME_ADMIN, "NvmeController.shutdown", self.admin);
        let timeout = self.controller_timeout();
        let cc = unsafe { ptr::read_volatile(&raw const (*self.regs).cc) };
        let cc = (cc & !regs::CC_SHN_MASK) | regs::CC_SHN_NORMAL;
        unsafe { ptr::write_volatile(&raw mut (*self.regs).cc, cc) };

        let start = Instant::now();
        loop {
            let csts = self.csts();
            if csts & regs::CSTS_SHST_MASK == regs::CSTS_SHST_COMPLETE {
                return Ok(());
            }
            if start.elapsed() > timeout {
                return Err(NvmeError::ControllerTimeout);
            }
            thread_sleep(Duration::from_millis(1));
        }
    }

    pub fn io_queue(&self) -> Option<&NvmeQueue> {
        self.io_queue.get()
    }

    /// Look up the queue a completion's `qid` belongs to: 0 is always the
    /// admin queue, `IO_QID` is this driver's one I/O queue once it exists.
    pub fn queue_for(&self, qid: u16) -> Option<&NvmeQueue> {
        match qid {
            0 => Some(&self.admin_queue),
            IO_QID => self.io_queue(),
            _ => None,
        }
    }

    /// Negotiate and create this driver's one I/O queue pair, with its CQ's
    /// interrupt vector (`IV`) set to 0 -- the same vector the admin queue
    /// uses -- so the dispatcher needs only the one IDT entry
    /// `configure_interrupt` bound.
    pub fn setup_io_queue(&self) -> Result<(), NvmeError> {
        self.set_num_queues()?;

        let dstrd = regs::cap_dstrd(self.cap);
        let bar_virt = x86_64::VirtAddr::new(self.regs as u64);

        // `CAP.MQES` is 0's-based and bounds every queue the controller will
        // create, not just the admin pair: asking for more fails Create I/O
        // CQ with "Invalid Queue Size" (NVMe base spec 2.0 5.2.1), which
        // this driver turns into no I/O queue at all rather than a smaller
        // working one. `NvmeController::new` has already refused anything
        // below the 32-entry admin queue, so the clamp cannot reach zero.
        // The cid bitmap moves with it: outstanding commands must stay
        // strictly below the ring size, which is what makes the SQ unable
        // to fill.
        let max_entries = regs::cap_mqes(self.cap).saturating_add(1);
        let sq_entries = IO_SQ_ENTRIES.min(max_entries);
        let cq_entries = IO_CQ_ENTRIES.min(max_entries);
        let cid_depth = IO_CID_DEPTH.min((sq_entries - 1).min(u8::MAX as u16) as u8);
        if sq_entries != IO_SQ_ENTRIES || cq_entries != IO_CQ_ENTRIES {
            log!(
                "nvme: MQES caps the I/O queue at {} SQ / {} CQ entries, {} outstanding",
                sq_entries,
                cq_entries,
                cid_depth
            );
        }

        let queue = NvmeQueue::new(IO_QID, sq_entries, cq_entries, cid_depth, dstrd, bar_virt)?;

        if let Err(e) = self.create_io_cq(&queue) {
            queue.dealloc_all();
            return Err(e);
        }
        if let Err(e) = self.create_io_sq(&queue) {
            // The CQ exists on the controller with `IEN` set and its pages
            // still named by the Create I/O CQ that installed them, so they
            // cannot go back to the shared DMA pool until the controller is
            // told to forget them. If it will not, leak them rather than
            // hand a live DMA target to the next allocation: `DmaBuffer`
            // has no `Drop`, so dropping the queue is exactly that leak.
            match self.delete_io_cq(queue.qid) {
                Ok(()) => queue.dealloc_all(),
                Err(e) => log!("nvme: Delete I/O CQ failed ({e}); leaking the queue's DMA pages"),
            }
            return Err(e);
        }

        self.io_queue.call_once(|| queue);
        Ok(())
    }

    /// Set Features: Number of Queues, requesting one I/O SQ and one I/O CQ
    /// (0's based, so CDW11 = 0). Logs what the controller actually granted.
    fn set_num_queues(&self) -> Result<(), NvmeError> {
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::ADMIN_OPC_SET_FEATURES, 0),
            nsid: 0,
            reserved: 0,
            mptr: 0,
            prp1: 0,
            prp2: 0,
            cdw10: FEATURE_NUMBER_OF_QUEUES,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        let dw0 = self.admin_command_polled(sqe)?;
        let nsqr = (dw0 & 0xFFFF) + 1;
        let ncqr = ((dw0 >> 16) & 0xFFFF) + 1;
        log!("nvme: granted {} I/O SQ(s), {} I/O CQ(s)", nsqr, ncqr);
        Ok(())
    }

    /// Create I/O Completion Queue (05h): `CDW11` sets `IV = 0` (bits
    /// 31:16), `IEN` (bit 1) and `PC` (bit 0, physically contiguous).
    fn create_io_cq(&self, queue: &NvmeQueue) -> Result<(), NvmeError> {
        let cdw10 = ((queue.cq_entries() as u32 - 1) << 16) | queue.qid as u32;
        let cdw11 = (1 << 1) | 1;
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::ADMIN_OPC_CREATE_IO_CQ, 0),
            nsid: 0,
            reserved: 0,
            mptr: 0,
            prp1: queue.cq_phys_addr().as_u64(),
            prp2: 0,
            cdw10,
            cdw11,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.admin_command_polled(sqe)?;
        Ok(())
    }

    /// Create I/O Submission Queue (01h): `CDW11` carries the owning CQ's
    /// id (bits 31:16, this driver's 1:1 pairing) and `PC` (bit 0).
    fn create_io_sq(&self, queue: &NvmeQueue) -> Result<(), NvmeError> {
        let cdw10 = ((queue.sq_entries() as u32 - 1) << 16) | queue.qid as u32;
        let cdw11 = ((queue.qid as u32) << 16) | 1;
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::ADMIN_OPC_CREATE_IO_SQ, 0),
            nsid: 0,
            reserved: 0,
            mptr: 0,
            prp1: queue.sq_phys_addr().as_u64(),
            prp2: 0,
            cdw10,
            cdw11,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.admin_command_polled(sqe)?;
        Ok(())
    }

    /// Delete I/O Completion Queue (04h): `CDW10` carries the queue id.
    /// Issued only to retire a CQ this driver created and is about to
    /// reclaim the memory of; the controller rejects it while any SQ is
    /// still associated with that CQ (NVMe base spec 2.0 5.5).
    fn delete_io_cq(&self, qid: u16) -> Result<(), NvmeError> {
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::ADMIN_OPC_DELETE_IO_CQ, 0),
            nsid: 0,
            reserved: 0,
            mptr: 0,
            prp1: 0,
            prp2: 0,
            cdw10: qid as u32,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.admin_command_polled(sqe)?;
        Ok(())
    }

    /// Submit `sqe` on the admin queue and poll for its completion, bounded
    /// by `CAP.TO`. Returns the completion's DW0 (command-specific result;
    /// reserved/zero for Identify, which returns its data through PRP1).
    pub fn admin_command_polled(&self, mut sqe: SubmissionQueueEntry) -> Result<u32, NvmeError> {
        let _guard = ranked_lock!(RANK_NVME_ADMIN, "NvmeController.admin", self.admin);

        let cid = self
            .admin_queue
            .alloc_cid()
            .expect("admin queue: cid exhausted with only one command ever outstanding");
        sqe.cdw0 = (sqe.cdw0 & 0x0000_FFFF) | ((cid as u32) << 16);
        self.admin_queue.write_sqe_and_ring(&sqe);

        let timeout = self.controller_timeout();
        let start = Instant::now();
        let mut result: Option<Result<u32, NvmeError>> = None;
        loop {
            self.admin_queue.drain(|completed_cid, status, dw0| {
                if completed_cid == cid as u16 {
                    self.admin_queue.free_cid(cid);
                    result = Some(
                        if regs::status_code(status) == 0 && regs::status_code_type(status) == 0 {
                            Ok(dw0)
                        } else {
                            Err(NvmeError::CommandFailed(status))
                        },
                    );
                }
            });
            if let Some(result) = result {
                return result;
            }
            if start.elapsed() > timeout {
                // The cid stays allocated. A timeout means the driver gave
                // up waiting, not that the controller dropped the command:
                // it may still post a completion, and NVMe 2.0 3.3.1
                // requires a command identifier be unique among those
                // outstanding on a queue, so reusing it invites the device
                // to write a completion that lands on an unrelated command.
                // The id is recovered by the controller reset that follows,
                // which reinitialises the whole queue.
                log!("nvme: admin command timed out, burning cid {}", cid);
                return Err(NvmeError::ControllerTimeout);
            }
            thread_sleep(Duration::from_millis(1));
        }
    }

    fn identify(&self, cns: u32, nsid: u32) -> Result<DmaBuffer, NvmeError> {
        let buffer = dma().allocate_sized(4096).map_err(NvmeError::DmaError)?;
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::ADMIN_OPC_IDENTIFY, 0),
            nsid,
            reserved: 0,
            mptr: 0,
            prp1: buffer.phys_addr().as_u64(),
            prp2: 0,
            cdw10: cns,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        // `DmaBuffer` has no `Drop`: returning the error without this
        // dealloc would strand the page for the life of the boot.
        if let Err(e) = self.admin_command_polled(sqe) {
            let _ = dma().dealloc(buffer);
            return Err(e);
        }
        Ok(buffer)
    }

    pub fn identify_controller(&self) -> Result<IdentifyController, NvmeError> {
        let buffer = self.identify(regs::IDENTIFY_CNS_CONTROLLER, 0)?;
        let ident = unsafe { *buffer.as_ptr().cast::<IdentifyController>() };
        dma().dealloc(buffer).map_err(NvmeError::DmaError)?;
        Ok(ident)
    }

    pub fn identify_namespace(&self, nsid: u32) -> Result<IdentifyNamespace, NvmeError> {
        let buffer = self.identify(regs::IDENTIFY_CNS_NAMESPACE, nsid)?;
        let ident = unsafe { *buffer.as_ptr().cast::<IdentifyNamespace>() };
        dma().dealloc(buffer).map_err(NvmeError::DmaError)?;
        Ok(ident)
    }

    /// The controller's active namespace ids, stopping at the list's first
    /// zero entry (NVM Command Set 5.17.2.1).
    pub fn active_namespace_ids(&self) -> Result<Vec<u32>, NvmeError> {
        let buffer = self.identify(regs::IDENTIFY_CNS_ACTIVE_NAMESPACE_LIST, 0)?;
        let mut ids = Vec::new();
        let list = buffer.as_ptr().cast::<u32>();
        for i in 0..1024 {
            let id = unsafe { ptr::read_volatile(list.add(i)) };
            if id == 0 {
                break;
            }
            ids.push(id);
        }
        dma().dealloc(buffer).map_err(NvmeError::DmaError)?;
        Ok(ids)
    }
}

/// Poll `CSTS.RDY` until it reads `want`, sleeping 1 ms between checks and
/// failing once `deadline` has elapsed.
fn wait_csts(
    regs: *mut NvmeRegs,
    bit: u32,
    want: bool,
    deadline: Duration,
) -> Result<(), NvmeError> {
    let start = Instant::now();
    loop {
        let csts = unsafe { ptr::read_volatile(&raw const (*regs).csts) };
        if (csts & bit != 0) == want {
            return Ok(());
        }
        if start.elapsed() > deadline {
            return Err(NvmeError::ControllerTimeout);
        }
        thread_sleep(Duration::from_millis(1));
    }
}
