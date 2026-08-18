//! One active namespace as an [`AsyncBlockDevice`] (NVM Command Set 3.2.2,
//! Read command 3.2.9).

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use x86_64::{VirtAddr, structures::paging::mapper::TranslateResult};

use crate::{
    drivers::{
        block_io::{AsyncBlockDevice, BlockBuffer, BlockError, BlockIoHandle, WriteFlags},
        dma::{DmaBuffer, dma},
        nvme::{
            NvmeError,
            admin::{IO_QID, NvmeController},
            cancel_op::{Direction, NvmeOp},
            queue::{PRP_LIST_ENTRIES, build_prp},
            regs::{self, SubmissionQueueEntry},
            status_to_block_error,
        },
    },
    log,
    memory::mapper::memory_mapper,
    thread::{
        cancel::ArcCancellableOp,
        scheduler::{current_thread, current_thread_weak},
    },
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

pub struct NvmeNamespace {
    controller: Arc<NvmeController>,
    nsid: u32,
    lba_count: u64,
    /// The id this namespace registers under in `block_io`
    /// (`3000 + controller_index * 64 + (nsid - 1)`).
    #[expect(dead_code, reason = "read once namespaces register with block_io")]
    device_id: u64,
}

impl NvmeNamespace {
    pub fn new(controller: Arc<NvmeController>, nsid: u32, lba_count: u64, device_id: u64) -> Self {
        Self {
            controller,
            nsid,
            lba_count,
            device_id,
        }
    }
}

impl AsyncBlockDevice for NvmeNamespace {
    fn submit_read(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let len = sectors as usize * 512;
        if sectors == 0
            || sectors > MAX_SECTORS_PER_COMMAND
            || buffer.len() < len
            || lba
                .checked_add(sectors as u64)
                .is_none_or(|end| end > self.lba_count)
        {
            return Err(BlockError::InvalidArg);
        }

        let queue = self.controller.io_queue().ok_or(BlockError::DeviceGone)?;

        // A caller's buffer is always a single contiguous virtual range
        // (Architecture Decision "PRP, and how it meets the NO_CACHE
        // cost"); translating its first page is enough to know whether the
        // dword-aligned, matching AHCI's own PRDT DBA rule.
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

        let cid = queue.alloc_cid_blocking();
        let submitter = current_thread_weak().unwrap_or_default();
        let handle = BlockIoHandle::pending();
        let op = Arc::new(NvmeOp::new(
            Arc::downgrade(&self.controller),
            IO_QID,
            cid,
            submitter,
            handle.clone(),
            buffer,
            Direction::Read,
            len,
            bounce,
            prp_list,
        ));

        // Install before issue: the doorbell must not be rung until the
        // dispatcher and cancel can already find this op by cid.
        queue.install_op(cid, Arc::clone(&op));
        if let Some(t) = current_thread()
            && t.owned_ops_push(Arc::clone(&op) as ArcCancellableOp)
                .is_err()
        {
            log!(
                "nvme: owned_ops full for cid {}; cancel hookup skipped",
                cid
            );
        }

        let sqe = SubmissionQueueEntry {
            cdw0: regs::cdw0(regs::NVM_OPC_READ, cid as u16),
            nsid: self.nsid,
            reserved: 0,
            mptr: 0,
            prp1,
            prp2,
            cdw10: (lba & 0xFFFF_FFFF) as u32,
            cdw11: (lba >> 32) as u32,
            cdw12: sectors - 1,
            cdw13: 0,
            cdw14: 0,
            cdw15: 0,
        };
        op.issue_time
            .store(Instant::now().as_nanos(), Ordering::Relaxed);
        queue.write_sqe_and_ring(&sqe);

        Ok(handle)
    }

    fn submit_write(
        &self,
        _lba: u64,
        _sectors: u32,
        _buffer: BlockBuffer,
        _flags: WriteFlags,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        // This driver has no write path; every write is refused.
        Err(BlockError::InvalidArg)
    }

    fn submit_flush(&self) -> Result<Arc<BlockIoHandle>, BlockError> {
        // This driver has no write path, so nothing needs flushing.
        Err(BlockError::InvalidArg)
    }

    fn sector_count(&self) -> u64 {
        self.lba_count
    }
}
