//! Controller bring-up (NVMe 2.0 3.5.1) and polled admin commands.

use core::{ptr, time::Duration};

use alloc::vec::Vec;
use x86_64::structures::paging::{PageTableFlags, mapper::MapToError};

use crate::{
    debug::lock_order::RANK_NVME_ADMIN,
    drivers::{
        dma::{DmaBuffer, dma},
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

pub struct NvmeController {
    #[expect(
        dead_code,
        reason = "read once MSI-X/MSI/INTx interrupt configuration runs"
    )]
    pub pci_device: PciDevice,
    #[expect(
        dead_code,
        reason = "read by the reset and shutdown register sequences"
    )]
    regs: *mut NvmeRegs,
    cap: u64,
    /// Serializes admin command issue and controller state transitions
    /// (init, queue create/delete, reset) so at most one is ever in flight.
    admin: BlockingMutex<()>,
    admin_queue: NvmeQueue,
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

        let aqa = ((ADMIN_CQ_ENTRIES as u32 - 1) << 16) | (ADMIN_SQ_ENTRIES as u32 - 1);
        unsafe {
            ptr::write_volatile(&raw mut (*regs).aqa, aqa);
            ptr::write_volatile(&raw mut (*regs).asq, admin_queue.sq_phys_addr().as_u64());
            ptr::write_volatile(&raw mut (*regs).acq, admin_queue.cq_phys_addr().as_u64());
        }

        let cc = regs::CC_EN
            | (0 << regs::CC_CSS_SHIFT)
            | (0 << regs::CC_MPS_SHIFT)
            | (6 << regs::CC_IOSQES_SHIFT) // 2^6 = 64 bytes
            | (4 << regs::CC_IOCQES_SHIFT); // 2^4 = 16 bytes
        unsafe { ptr::write_volatile(&raw mut (*regs).cc, cc) };
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

        Ok(Self {
            pci_device,
            regs,
            cap,
            admin: BlockingMutex::new(()),
            admin_queue,
        })
    }

    pub fn cap(&self) -> u64 {
        self.cap
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

        let timeout =
            Duration::from_millis(regs::cap_to(self.cap) as u64 * 500).max(Duration::from_secs(1));
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
