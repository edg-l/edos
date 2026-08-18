//! One submission/completion queue pair: doorbell rings, the command-id
//! bitmap, and the completion drain pass.

use core::{
    ptr,
    sync::atomic::{AtomicU64, Ordering},
};

use x86_64::{PhysAddr, VirtAddr};

use crate::{
    debug::lock_order::{RANK_NVME_CQ, RANK_NVME_SQ},
    drivers::{
        dma::{DmaBuffer, dma},
        nvme::{
            NvmeError,
            regs::{self, CompletionQueueEntry, SubmissionQueueEntry},
        },
    },
    ranked_lock,
};

struct SqState {
    buffer: DmaBuffer,
    entries: u16,
    tail: u16,
}

struct CqState {
    buffer: DmaBuffer,
    entries: u16,
    head: u16,
    phase: bool,
}

/// One submission/completion queue pair (admin, or one I/O pair).
///
/// Command ids come from a single 64-bit bitmap capped at `cid_depth`, so the
/// SQ (up to 128 entries) can never fill: outstanding commands are bounded by
/// the bitmap width, not the ring size, and there is no ring-full path to
/// handle.
pub struct NvmeQueue {
    #[expect(
        dead_code,
        reason = "read by Create I/O SQ/CQ and the dispatcher's per-queue logging"
    )]
    pub qid: u16,
    sq: spin::Mutex<SqState>,
    cq: spin::Mutex<CqState>,
    free_cids: AtomicU64,
    cid_depth: u8,
    sq_phys: PhysAddr,
    cq_phys: PhysAddr,
    sq_tail_doorbell: *mut u32,
    cq_head_doorbell: *mut u32,
}

// SAFETY: the doorbell pointers are stable MMIO addresses for the lifetime
// of the mapping and are only ever written through, never aliased mutably
// by Rust references; the DMA buffers behind `sq`/`cq` are exclusively owned.
unsafe impl Send for NvmeQueue {}
unsafe impl Sync for NvmeQueue {}

impl NvmeQueue {
    /// `bar_virt` is the mapped `BAR0` base; `dstrd` is `CAP.DSTRD`, needed
    /// to place this queue's doorbells.
    pub fn new(
        qid: u16,
        sq_entries: u16,
        cq_entries: u16,
        cid_depth: u8,
        dstrd: u8,
        bar_virt: VirtAddr,
    ) -> Result<Self, NvmeError> {
        let sq_bytes = sq_entries as usize * core::mem::size_of::<SubmissionQueueEntry>();
        let sq_buffer = dma()
            .allocate_sized_uninit(sq_bytes)
            .map_err(NvmeError::DmaError)?;

        let cq_bytes = cq_entries as usize * core::mem::size_of::<CompletionQueueEntry>();
        // `DmaBuffer` has no `Drop`, so a buffer dropped without an explicit
        // `dealloc` is lost for the life of the boot rather than recycled.
        let cq_buffer = match dma().allocate_sized_uninit(cq_bytes) {
            Ok(buffer) => buffer,
            Err(e) => {
                let _ = dma().dealloc(sq_buffer);
                return Err(NvmeError::DmaError(e));
            }
        };
        // `allocate_sized_uninit` zeroes nothing, and a buffer served from a
        // recycled bucket keeps its previous owner's bytes. The completion
        // queue's phase bit protocol assumes phase 1 with every entry
        // reading phase 0 at start of day; stale bytes here invent
        // completions on the first drain pass rather than waiting for real
        // ones.
        unsafe {
            ptr::write_bytes(cq_buffer.as_ptr(), 0, cq_buffer.size);
        }

        let sq_phys = sq_buffer.phys_addr();
        let cq_phys = cq_buffer.phys_addr();

        let sq_tail_doorbell = unsafe {
            bar_virt
                .as_mut_ptr::<u8>()
                .add(regs::sq_tail_doorbell_offset(qid, dstrd))
                .cast::<u32>()
        };
        let cq_head_doorbell = unsafe {
            bar_virt
                .as_mut_ptr::<u8>()
                .add(regs::cq_head_doorbell_offset(qid, dstrd))
                .cast::<u32>()
        };

        Ok(Self {
            qid,
            sq: spin::Mutex::new(SqState {
                buffer: sq_buffer,
                entries: sq_entries,
                tail: 0,
            }),
            cq: spin::Mutex::new(CqState {
                buffer: cq_buffer,
                entries: cq_entries,
                head: 0,
                phase: true,
            }),
            free_cids: AtomicU64::new(0),
            cid_depth,
            sq_phys,
            cq_phys,
            sq_tail_doorbell,
            cq_head_doorbell,
        })
    }

    pub fn sq_phys_addr(&self) -> PhysAddr {
        self.sq_phys
    }

    pub fn cq_phys_addr(&self) -> PhysAddr {
        self.cq_phys
    }

    /// Claim the lowest free command id below `cid_depth`, or `None` if
    /// every one is outstanding.
    pub fn alloc_cid(&self) -> Option<u8> {
        let mask = if self.cid_depth >= 64 {
            u64::MAX
        } else {
            (1u64 << self.cid_depth) - 1
        };
        loop {
            let cur = self.free_cids.load(Ordering::Acquire);
            let free = !cur & mask;
            if free == 0 {
                return None;
            }
            let cid = free.trailing_zeros() as u8;
            let next = cur | (1 << cid);
            if self
                .free_cids
                .compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(cid);
            }
        }
    }

    pub fn free_cid(&self, cid: u8) {
        self.free_cids.fetch_and(!(1u64 << cid), Ordering::AcqRel);
    }

    /// Write `sqe` at the current tail and ring the SQ tail doorbell.
    pub fn write_sqe_and_ring(&self, sqe: &SubmissionQueueEntry) {
        let mut sq = ranked_lock!(RANK_NVME_SQ, "NvmeQueue.sq", self.sq);
        let idx = sq.tail as usize;
        unsafe {
            sq.buffer
                .as_ptr()
                .cast::<SubmissionQueueEntry>()
                .add(idx)
                .write_volatile(*sqe);
        }
        sq.tail = (sq.tail + 1) % sq.entries;
        unsafe {
            ptr::write_volatile(self.sq_tail_doorbell, sq.tail as u32);
        }
    }

    /// Walk the CQ from its current head, collecting every entry whose phase
    /// bit matches the queue's expected phase, then ring the CQ head
    /// doorbell once and call `f(cid, status, dw0)` for each.
    ///
    /// The collect-then-dispatch split is required, not a style choice:
    /// `f` typically takes the per-command-id lock (`RANK_NVME_CMD`) and may
    /// copy a bounced read back into the caller's buffer, and both of those
    /// must happen with the CQ lock (`RANK_NVME_CQ`) released -- taking
    /// `RANK_NVME_CMD` while holding `RANK_NVME_CQ` would be a descending
    /// acquisition. The 64-entry bound is the command-id bitmap's width, so
    /// it cannot overflow; `push` still asserts rather than silently
    /// dropping a completion, since a dropped one leaks its cid and leaves
    /// its waiter parked forever.
    /// Return both queue buffers to the DMA allocator. Callers that fail
    /// part-way through controller bring-up must invoke this: `DmaBuffer`
    /// has no `Drop`, so dropping the queue alone leaks both regions.
    pub fn dealloc_all(self) {
        let sq = self.sq.into_inner();
        let cq = self.cq.into_inner();
        let _ = dma().dealloc(sq.buffer);
        let _ = dma().dealloc(cq.buffer);
    }

    pub fn drain(&self, mut f: impl FnMut(u16, u16, u32)) {
        let mut completions: heapless::Vec<(u16, u16, u32), 64> = heapless::Vec::new();
        {
            let mut cq = ranked_lock!(RANK_NVME_CQ, "NvmeQueue.cq", self.cq);
            loop {
                let idx = cq.head as usize;
                let entry = unsafe {
                    ptr::read_volatile(cq.buffer.as_ptr().cast::<CompletionQueueEntry>().add(idx))
                };
                if regs::cqe_phase(entry.dw3) != cq.phase {
                    break;
                }
                let cid = regs::cqe_cid(entry.dw3);
                let status = (entry.dw3 >> 16) as u16;
                completions
                    .push((cid, status, entry.dw0))
                    .expect("cq drain: more completions than outstanding cids");
                cq.head += 1;
                if cq.head == cq.entries {
                    cq.head = 0;
                    cq.phase = !cq.phase;
                }
            }
            if !completions.is_empty() {
                unsafe {
                    ptr::write_volatile(self.cq_head_doorbell, cq.head as u32);
                }
            }
        }
        for (cid, status, dw0) in completions {
            f(cid, status, dw0);
        }
    }
}
