//! One active namespace as an [`AsyncBlockDevice`] (NVM Command Set 3.2.2,
//! Read command 3.2.9, Write 3.2.11, Flush 3.2.2).

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use x86_64::{VirtAddr, structures::paging::mapper::TranslateResult};

use crate::{
    drivers::{
        block_io::{AsyncBlockDevice, BlockBuffer, BlockError, BlockIoHandle, WriteFlags},
        dma::{DmaBuffer, dma},
        nvme::{
            NvmeError,
            admin::{IO_QID, NvmeController},
            cancel_op::{Completion, Direction, NvmeOp, OpPayload, SplitOp},
            queue::{NvmeQueue, PRP_LIST_ENTRIES, build_prp},
            regs::{self, SubmissionQueueEntry},
            stats, status_to_block_error, watchdog,
        },
    },
    log,
    memory::mapper::memory_mapper,
    thread::{cancel::ArcCancellableOp, scheduler::current_thread},
    timer::Instant,
};

/// The largest single command this driver issues. NLB (`CDW12` bits 15:0) is
/// 0's based, so the command format itself allows 65536 sectors, but
/// [`build_prp`] describes a transfer with one PRP list page, and 512 entries
/// of 4096 bytes is 2 MiB. Admitting more only reaches a `build_prp` that
/// refuses -- and reaches it after the bounce path has already allocated and
/// contiguously mapped the whole request, which for the format's own maximum
/// is a 32 MiB run of frames asked for and thrown away.
const MAX_SECTORS_PER_COMMAND: u32 = (PRP_LIST_ENTRIES * 4096 / 512) as u32;

fn nvme_err_to_block(e: NvmeError) -> BlockError {
    match e {
        NvmeError::InvalidDevice => BlockError::DeviceGone,
        NvmeError::DmaError(_) => BlockError::NoMemory,
        NvmeError::ControllerTimeout => BlockError::Timeout,
        NvmeError::CommandFailed(status) => status_to_block_error(status)
            .err()
            .unwrap_or(BlockError::Io),
        NvmeError::Unsupported => BlockError::InvalidArg,
    }
}

/// What one read or write command transfers. Bundled because the four travel
/// together through the split loop and back, and separating them from the
/// buffer and the completion is what keeps `issue_transfer` readable at its
/// call sites.
#[derive(Clone, Copy)]
struct Transfer {
    lba: u64,
    sectors: u32,
    direction: Direction,
    fua: bool,
}

pub struct NvmeNamespace {
    controller: Arc<NvmeController>,
    nsid: u32,
    lba_count: u64,
    /// The id this namespace registers under in `block_io`
    /// (`3000 + controller_index * 64 + (nsid - 1)`).
    device_id: u64,
    /// MDTS resolved to bytes at probe (NVMe 2.0 Identify Controller byte
    /// 77 against `CAP.MPSMIN`). A request longer than this is split into
    /// several commands rather than refused.
    max_transfer_bytes: usize,
    /// `Identify Controller` VWC bit 0: the controller has a volatile write
    /// cache, so a flush has something to do.
    write_cache: bool,
}

impl NvmeNamespace {
    pub fn new(
        controller: Arc<NvmeController>,
        nsid: u32,
        lba_count: u64,
        device_id: u64,
        max_transfer_bytes: usize,
        write_cache: bool,
    ) -> Self {
        Self {
            controller,
            nsid,
            lba_count,
            device_id,
            max_transfer_bytes,
            write_cache,
        }
    }

    /// The `block_io` id this namespace is registered under.
    /// The controller's `MDTS` in bytes, as `/proc/nvme_stats` reports it.
    pub fn max_transfer_bytes(&self) -> usize {
        self.max_transfer_bytes
    }

    /// Whether the controller reported a volatile write cache, which is
    /// what decides if a flush issues a command or is elided.
    pub fn write_cache(&self) -> bool {
        self.write_cache
    }

    pub fn device_id(&self) -> u64 {
        self.device_id
    }

    /// Sectors one device command may carry: the smaller of what the
    /// controller admits (MDTS) and what one PRP list page can describe.
    fn max_sectors_per_command(&self) -> u32 {
        let by_mdts = (self.max_transfer_bytes / 512).min(u32::MAX as usize) as u32;
        by_mdts.clamp(1, MAX_SECTORS_PER_COMMAND)
    }

    /// Build the PRP pair for `buffer`, bouncing through a DMA allocation
    /// when the caller's own pages cannot be described. On the bounce path
    /// for a write, the caller's bytes are copied in here, before anything
    /// is installed or rung -- the device may read the buffer the instant
    /// the doorbell is written.
    ///
    /// Every early return reclaims what it allocated: `DmaBuffer` has no
    /// `Drop`, so a leaked one is gone for the boot.
    fn build_transfer(
        buffer: &BlockBuffer,
        len: usize,
        direction: Direction,
    ) -> Result<(u64, u64, Option<DmaBuffer>, Option<DmaBuffer>), BlockError> {
        // A caller's buffer is always a single contiguous virtual range
        // (Architecture Decision "PRP, and how it meets the NO_CACHE
        // cost"), so translating its first page is enough to learn where
        // the transfer physically begins. PRP1 must be dword-aligned (NVM
        // Command Set 3.3.1), matching AHCI's own PRDT DBA rule, and a
        // start that is not goes down the bounce path with everything else
        // `build_prp` cannot describe.
        let vaddr = VirtAddr::new(buffer.as_ptr() as u64);
        let dword_aligned = match memory_mapper().translate(vaddr) {
            TranslateResult::Mapped { frame, offset, .. } => {
                (frame.start_address() + offset).as_u64().is_multiple_of(4)
            }
            _ => false,
        };

        // Try the caller's own pages first. `build_prp` translates each one
        // and refuses if any is unmapped, so the bounce covers both that and
        // a misaligned start; a `dma()` buffer is contiguous and page-aligned
        // by construction.
        let mut prp_list = None;
        let mut bounce: Option<DmaBuffer> = None;
        let (prp1, prp2) = match dword_aligned
            .then(|| build_prp(vaddr, len, &mut prp_list))
            .transpose()
        {
            Ok(Some(prps)) => prps,
            Ok(None) | Err(_) => {
                if let Some(list) = prp_list.take() {
                    let _ = dma().dealloc(list);
                }
                let b = dma()
                    .allocate_sized_uninit(len)
                    .map_err(|_| BlockError::NoMemory)?;
                let bounce_vaddr = VirtAddr::new(b.as_ptr() as u64);
                match build_prp(bounce_vaddr, len, &mut prp_list) {
                    Ok(prps) => {
                        if direction == Direction::Write {
                            // SAFETY: `b` was allocated with exactly `len`
                            // bytes, and the caller's buffer was validated
                            // to be at least `len` before this call. The
                            // two allocations never overlap.
                            unsafe {
                                core::ptr::copy_nonoverlapping(buffer.as_ptr(), b.as_ptr(), len);
                            }
                        }
                        stats::bump(&stats::BOUNCED_REQUESTS, 1);
                        bounce = Some(b);
                        prps
                    }
                    Err(e) => {
                        if let Some(list) = prp_list.take() {
                            let _ = dma().dealloc(list);
                        }
                        let _ = dma().dealloc(b);
                        return Err(nvme_err_to_block(e));
                    }
                }
            }
        };
        Ok((prp1, prp2, bounce, prp_list))
    }

    /// Install `op` in the queue's slot table and the submitter's cancel
    /// list, then stamp and ring. Split out from the callers only because
    /// three command paths repeat it verbatim.
    fn issue(&self, queue: &NvmeQueue, op: &Arc<NvmeOp>, sqe: &SubmissionQueueEntry) {
        // Install before issue: the doorbell must not be rung until the
        // dispatcher and cancel can already find this op by cid.
        queue.install_op(op.cid, Arc::clone(op));
        if let Some(t) = current_thread()
            && t.owned_ops_push(Arc::clone(op) as ArcCancellableOp)
                .is_err()
        {
            log!(
                "nvme: owned_ops full for cid {}; cancel hookup skipped",
                op.cid
            );
        }
        op.issue_time
            .store(Instant::now().as_nanos(), Ordering::Relaxed);
        watchdog::inflight_inc();
        queue.write_sqe_and_ring(sqe);
        stats::bump(&stats::COMMANDS_SUBMITTED, 1);
    }

    /// One read or write command, no larger than [`Self::max_sectors_per_command`].
    fn issue_transfer(
        &self,
        queue: &NvmeQueue,
        xfer: Transfer,
        buffer: BlockBuffer,
        completion: Completion,
    ) -> Result<(), BlockError> {
        let Transfer {
            lba,
            sectors,
            direction,
            fua,
        } = xfer;
        let len = sectors as usize * 512;
        let (prp1, prp2, bounce, prp_list) = Self::build_transfer(&buffer, len, direction)?;

        let cid = queue.alloc_cid_blocking();
        let op = Arc::new(NvmeOp::new(
            Arc::downgrade(&self.controller),
            IO_QID,
            cid,
            OpPayload {
                completion,
                buffer,
                direction,
                len,
                bounce,
                prp_list,
            },
        ));

        let opcode = match direction {
            Direction::Read => regs::NVM_OPC_READ,
            _ => regs::NVM_OPC_WRITE,
        };
        // CDW12: NLB in bits 15:0 (0's based), FUA in bit 30 (NVM Command
        // Set 1.0 3.3.7): the command is not complete until the data is on
        // non-volatile media, whatever the write cache would otherwise do.
        let cdw12 = (sectors - 1) | if fua { 1 << 30 } else { 0 };
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(opcode, cid as u16),
            nsid: self.nsid,
            reserved: 0,
            mptr: 0,
            prp1,
            prp2,
            cdw10: (lba & 0xFFFF_FFFF) as u32,
            cdw11: (lba >> 32) as u32,
            cdw12,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.issue(queue, &op, &sqe);
        Ok(())
    }

    /// Read and write differ only in opcode, in which way the bounce buffer
    /// is copied, and in whether FUA means anything, so they share
    /// everything else -- including the chopping of a request larger than
    /// one command into parts that report to one shared handle.
    fn submit_transfer(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
        direction: Direction,
        fua: bool,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let len = sectors as usize * 512;
        if sectors == 0
            || buffer.len() < len
            || lba
                .checked_add(sectors as u64)
                .is_none_or(|end| end > self.lba_count)
        {
            return Err(BlockError::InvalidArg);
        }

        let queue = self.controller.io_queue().ok_or(BlockError::DeviceGone)?;
        let handle = BlockIoHandle::pending();
        let max_sectors = self.max_sectors_per_command();

        if sectors <= max_sectors {
            self.issue_transfer(
                queue,
                Transfer {
                    lba,
                    sectors,
                    direction,
                    fua,
                },
                buffer,
                Completion::Whole(Arc::clone(&handle)),
            )?;
            return Ok(handle);
        }

        let parts = sectors.div_ceil(max_sectors);
        let split = Arc::new(SplitOp::new(Arc::clone(&handle), parts));
        stats::bump(&stats::SPLIT_REQUESTS, 1);
        stats::bump(&stats::SPLIT_COMMANDS, u64::from(parts));
        for part in 0..parts {
            let first = part * max_sectors;
            let part_sectors = max_sectors.min(sectors - first);
            let part_buffer = buffer.subrange(first as usize * 512, part_sectors as usize * 512);
            if let Err(e) = self.issue_transfer(
                queue,
                Transfer {
                    lba: lba + u64::from(first),
                    sectors: part_sectors,
                    direction,
                    fua,
                },
                part_buffer,
                Completion::Part(Arc::clone(&split)),
            ) {
                // The parts already issued still complete and reclaim
                // themselves; the ones never issued are accounted here in
                // one step so the counter still reaches zero. The error
                // reaches the caller through the handle rather than as an
                // `Err` return, because part of the request is in flight
                // and the caller must still wait for it.
                split.parts_done(parts - part, Err(e));
                break;
            }
        }
        Ok(handle)
    }
}

impl AsyncBlockDevice for NvmeNamespace {
    fn submit_read(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        self.submit_transfer(lba, sectors, buffer, Direction::Read, false)
    }

    fn submit_write(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
        flags: WriteFlags,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        self.submit_transfer(
            lba,
            sectors,
            buffer,
            Direction::Write,
            flags.contains(WriteFlags::FUA),
        )
    }

    fn submit_flush(&self) -> Result<Arc<BlockIoHandle>, BlockError> {
        let handle = BlockIoHandle::pending();
        // NVM Command Set 1.0 3.2.2: Flush commits the volatile write cache
        // to non-volatile media. A controller reporting VWC bit 0 clear has
        // no such cache, and the trait allows the no-op the absence implies.
        if !self.write_cache {
            stats::bump(&stats::FLUSHES_ELIDED, 1);
            handle.complete(Ok(()));
            return Ok(handle);
        }

        let queue = self.controller.io_queue().ok_or(BlockError::DeviceGone)?;
        let cid = queue.alloc_cid_blocking();
        let op = Arc::new(NvmeOp::new(
            Arc::downgrade(&self.controller),
            IO_QID,
            cid,
            OpPayload {
                completion: Completion::Whole(Arc::clone(&handle)),
                // Flush transfers no data, so the op's buffer exists only to
                // satisfy the shared shape: zero length, nothing to copy back.
                buffer: BlockBuffer::owned_vec(Arc::new(Vec::new())),
                direction: Direction::Flush,
                len: 0,
                bounce: None,
                prp_list: None,
            },
        ));
        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::NVM_OPC_FLUSH, cid as u16),
            nsid: self.nsid,
            reserved: 0,
            mptr: 0,
            prp1: 0,
            prp2: 0,
            cdw10: 0,
            cdw11: 0,
            cdw12: 0,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        self.issue(queue, &op, &sqe);
        stats::bump(&stats::FLUSHES, 1);
        Ok(handle)
    }

    fn sector_count(&self) -> u64 {
        self.lba_count
    }
}
