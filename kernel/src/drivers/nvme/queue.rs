//! One submission/completion queue pair: doorbell rings, the command-id
//! bitmap, and the completion drain pass.

use core::{
    ptr,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};

use alloc::{sync::Arc, vec::Vec};
use x86_64::{PhysAddr, VirtAddr, structures::paging::mapper::TranslateResult};

use crate::{
    debug::lock_order::{RANK_NVME_CMD, RANK_NVME_CQ, RANK_NVME_SQ},
    drivers::{
        block_io::BlockError,
        dma::{DmaBuffer, dma},
        nvme::{
            FAIL_ALL_PASSES, NvmeError, Retire,
            cancel_op::NvmeOp,
            regs::{self, CompletionQueueEntry, SubmissionQueueEntry},
            stats,
        },
    },
    log,
    memory::mapper::memory_mapper,
    ranked_lock,
    thread::waitqueue::WaitQueue,
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

/// Pack the completion queue's head and phase into one word. Read back by
/// [`NvmeQueue::has_pending_lockfree`]; see `cq_cursor` for why the pair must
/// travel together.
const fn cq_cursor(head: u16, phase: bool) -> u32 {
    ((phase as u32) << 16) | head as u32
}

/// One submission/completion queue pair (admin, or one I/O pair).
///
/// Command ids come from a single 64-bit bitmap capped at `cid_depth`, so the
/// SQ (up to 128 entries) can never fill: outstanding commands are bounded by
/// the bitmap width, not the ring size, and there is no ring-full path to
/// handle.
pub struct NvmeQueue {
    pub qid: u16,
    sq: spin::Mutex<SqState>,
    cq: spin::Mutex<CqState>,
    /// Per-command-id op handoff between submit, the IRQ dispatcher, the
    /// watchdog and cancel. Indexed by cid, sized to `cid_depth` rather than
    /// the (larger) ring size.
    cmd_slots: Vec<spin::Mutex<Option<Arc<NvmeOp>>>>,
    /// Where a submitter parks when every cid below `cid_depth` is
    /// outstanding. Woken by `free_cid`.
    cid_waitq: WaitQueue,
    free_cids: AtomicU64,
    /// The completion queue's `(phase << 16) | head`, republished on every
    /// change to `cq`. One atomic rather than two so a reader gets a
    /// *consistent* pair: a head from before an update paired with a phase
    /// from after it would name an entry whose phase bit means the opposite
    /// of what the reader concludes.
    ///
    /// Exists so [`Self::has_pending_lockfree`] can answer without taking
    /// `cq` (rank 186), which a park predicate must not do.
    cq_cursor: AtomicU32,
    /// Base of the completion queue ring. Stable for the queue's life --
    /// `reset_state` re-zeroes the buffer but never moves it -- so a reader
    /// outside the lock can index it.
    cq_ring: *const CompletionQueueEntry,
    cid_depth: u8,
    sq_entries: u16,
    cq_entries: u16,
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

        let cmd_slots = (0..cid_depth).map(|_| spin::Mutex::new(None)).collect();

        let sq_phys = sq_buffer.phys_addr();
        let cq_phys = cq_buffer.phys_addr();
        let cq_ring = cq_buffer
            .as_ptr()
            .cast::<CompletionQueueEntry>()
            .cast_const();

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
            cmd_slots,
            cid_waitq: WaitQueue::new(),
            free_cids: AtomicU64::new(0),
            cq_cursor: AtomicU32::new(cq_cursor(0, true)),
            cq_ring,
            cid_depth,
            sq_entries,
            cq_entries,
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

    /// Entry count this queue's SQ was created with, for `Create I/O SQ`'s
    /// CDW10 size field.
    pub fn sq_entries(&self) -> u16 {
        self.sq_entries
    }

    /// Entry count this queue's CQ was created with, for `Create I/O CQ`'s
    /// CDW10 size field.
    pub fn cq_entries(&self) -> u16 {
        self.cq_entries
    }

    /// Bitmask covering the `cid_depth` valid command ids.
    fn cid_mask(&self) -> u64 {
        if self.cid_depth >= 64 {
            u64::MAX
        } else {
            (1u64 << self.cid_depth) - 1
        }
    }

    /// Claim the lowest free command id below `cid_depth`, or `None` if
    /// every one is outstanding.
    pub fn alloc_cid(&self) -> Option<u8> {
        let mask = self.cid_mask();
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

    /// Claim a command id, parking on `cid_waitq` while every one below
    /// `cid_depth` is outstanding. Mirrors AHCI's `allocate_slot_blocking`.
    pub fn alloc_cid_blocking(&self) -> u8 {
        loop {
            if let Some(cid) = self.alloc_cid() {
                return cid;
            }
            let mask = self.cid_mask();
            self.cid_waitq
                .wait_until(|| self.free_cids.load(Ordering::Acquire) & mask != mask);
        }
    }

    pub fn free_cid(&self, cid: u8) {
        self.free_cids.fetch_and(!(1u64 << cid), Ordering::AcqRel);
        self.cid_waitq.wake_one();
    }

    /// Install `op` at `cid`'s slot for the IRQ dispatcher, the watchdog and
    /// cancel to find it by command id.
    ///
    /// Two things are counted here rather than assumed, because a submitter
    /// holds its command id across a heap allocation and an SQE build, and a
    /// reset in that window declares every id free (`reset_state`). If it then
    /// hands this one to a second submitter, the op already in the slot is
    /// dropped without ever completing its handle and its waiter parks for
    /// good -- with nothing outstanding, so the watchdog sweeps silently and
    /// the machine simply goes idle. Both counters must stay zero.
    pub fn install_op(&self, cid: u8, op: Arc<NvmeOp>) {
        if self.free_cids.load(Ordering::Acquire) & (1u64 << cid) == 0 {
            stats::bump(&stats::CID_NOT_HELD_AT_INSTALL, 1);
        }
        let previous = ranked_lock!(
            RANK_NVME_CMD,
            "NvmeQueue.cmd_slots",
            self.cmd_slots[cid as usize]
        )
        .replace(op);
        if let Some(lost) = previous {
            stats::bump(&stats::SLOT_OVERWRITTEN, 1);
            log!(
                "nvme: cid {} installed over a live op on queue {}",
                cid,
                lost.qid
            );
        }
    }

    /// Clone the op installed at `cid`, or `None` if the slot is empty (a
    /// concurrent completion path already reclaimed it).
    pub fn cmd_slot(&self, cid: u8) -> Option<Arc<NvmeOp>> {
        ranked_lock!(
            RANK_NVME_CMD,
            "NvmeQueue.cmd_slots",
            self.cmd_slots[cid as usize]
        )
        .clone()
    }

    /// Clear `cid`'s slot. Called once the completion path has won the
    /// reclaim transition on the op's own state.
    pub fn clear_cmd_slot(&self, cid: u8) {
        **ranked_lock!(
            RANK_NVME_CMD,
            "NvmeQueue.cmd_slots",
            self.cmd_slots[cid as usize]
        ) = None;
    }

    /// Every op currently installed, cloned out. The watchdog's view of
    /// what is outstanding: each slot is locked and released in turn, so
    /// this holds no lock while the caller works on the result.
    pub fn outstanding_ops(&self) -> Vec<Arc<NvmeOp>> {
        (0..self.cid_depth)
            .filter_map(|cid| self.cmd_slot(cid))
            .collect()
    }

    /// Whether every command slot is free. The precondition a controller
    /// reset asserts, since a reset clears the slots and would strand the
    /// DMA memory of anything still installed.
    pub fn cmd_slots_empty(&self) -> bool {
        self.cmd_slots
            .iter()
            .all(|slot| ranked_lock!(RANK_NVME_CMD, "NvmeQueue.cmd_slots", slot).is_none())
    }

    /// Return this queue to its start-of-day state for a controller reset:
    /// empty rings, phase 1, no allocated command ids.
    ///
    /// The completion queue buffer is re-zeroed explicitly. The phase-bit
    /// protocol restarts from phase 1 expecting every entry to read phase
    /// 0, and this buffer is not fresh: entries left from before the reset
    /// would be read as completions on the first drain pass.
    ///
    /// Locks are taken one at a time and in rank order (`cmd_slots` 182,
    /// `cq` 186, `sq` 192); nothing here needs two of them at once.
    pub fn reset_state(&self) {
        // Every slot is emptied by TAKING its op and retiring it, never by
        // overwriting it with `None`. Scanning and then clearing in two
        // passes is what this used to do, and a command installed between
        // the two was dropped rather than retired: no `inflight_dec`, no
        // command id freed, no handle completed, so its submitter parked on
        // that handle for good. That window is not rare, it is the busy one
        // -- the caller's fail-all pass has just woken every waiter with
        // `Io`, and `block_io`'s retry re-issues from those threads
        // immediately.
        //
        // `retire_op` returns without touching anything if the op is already
        // terminal, so a slot whose op lost the reclaim race to a concurrent
        // completion costs one no-op call rather than a stranded buffer.
        // Looped because a submitter can install into a slot this pass has
        // already walked past.
        for _ in 0..FAIL_ALL_PASSES {
            let mut emptied = 0u64;
            for slot in &self.cmd_slots {
                let op = ranked_lock!(RANK_NVME_CMD, "NvmeQueue.cmd_slots", slot).take();
                if let Some(op) = op {
                    emptied += 1;
                    crate::drivers::nvme::retire_op(
                        self,
                        op,
                        Err(BlockError::Io),
                        Retire::Abandoned,
                    );
                }
            }
            if emptied == 0 {
                break;
            }
            stats::bump(&stats::INSTALLED_DURING_RESET, emptied);
        }
        {
            let mut cq = ranked_lock!(RANK_NVME_CQ, "NvmeQueue.cq", self.cq);
            cq.head = 0;
            cq.phase = true;
            unsafe {
                ptr::write_bytes(cq.buffer.as_ptr(), 0, cq.buffer.size);
            }
            self.cq_cursor
                .store(cq_cursor(cq.head, cq.phase), Ordering::Release);
        }
        {
            let mut sq = ranked_lock!(RANK_NVME_SQ, "NvmeQueue.sq", self.sq);
            sq.tail = 0;
        }
        self.free_cids.store(0, Ordering::Release);
        // A submitter parked on an exhausted queue would otherwise wait for
        // a `free_cid` that is never coming: the ops it was waiting behind
        // were failed, not completed.
        self.cid_waitq.wake_all();
    }

    /// Whether the entry at the completion queue's head carries the phase a
    /// drain would accept -- that is, whether the device has posted work this
    /// driver has not consumed. Takes no lock.
    ///
    /// This is the dispatcher's park predicate, and it has to be the *queue*
    /// rather than an interrupt counter. "No interrupt since I last looked"
    /// is not the same claim as "the queue is empty": an MSI-X message
    /// coalesced with one already counted leaves a completion sitting here
    /// with the counter unchanged, and a dispatcher parked on the counter
    /// sleeps in front of work it could see, until the watchdog's sweep a
    /// second later. That is worth roughly a 100 ms stall per occurrence in
    /// practice -- the idle CPU's fallback timer -- and it is what made a
    /// raw NVMe sweep slower than AHCI despite a better median.
    ///
    /// Staleness is safe in one direction only, which is the direction it
    /// falls: a cursor read from before a drain names an entry that drain
    /// already consumed, whose phase still matches the phase read with it, so
    /// the answer is a spurious `true` and costs one empty pass. It cannot
    /// produce a spurious `false`, which would be a missed wake.
    pub fn has_pending_lockfree(&self) -> bool {
        let cursor = self.cq_cursor.load(Ordering::Acquire);
        let head = (cursor & 0xFFFF) as usize;
        let phase = (cursor >> 16) != 0;
        // SAFETY: `cq_ring` is the base of a `cq_entries`-long ring that
        // lives as long as this queue, and `head` is always below that count
        // because every writer wraps it. The read races the device's own DMA
        // writes by design -- exactly as `drain`'s does -- which is why it is
        // volatile and why the phase bit, not the payload, is what it trusts.
        let entry = unsafe { ptr::read_volatile(self.cq_ring.add(head)) };
        regs::cqe_phase(entry.dw3) == phase
    }

    /// Number of command ids `cmd_slots` was sized for, so a caller can
    /// range-check a completion's cid before indexing.
    pub fn cid_depth(&self) -> u8 {
        self.cid_depth
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
            // Republished under the lock, so a reader outside it never sees a
            // cursor newer than the ring it describes.
            self.cq_cursor
                .store(cq_cursor(cq.head, cq.phase), Ordering::Release);
        }
        for (cid, status, dw0) in completions {
            f(cid, status, dw0);
        }
    }
}

/// One list page's worth of PRP entries: 4096 / 8 bytes each.
pub const PRP_LIST_ENTRIES: usize = 512;

/// Build PRP1/PRP2 for a `len`-byte transfer starting at `vaddr`
/// (NVM Command Set 3.3.1).
///
/// PRP1 carries its own byte offset into the first page; every entry after
/// it addresses a whole page and must be page-aligned. Up to two pages need
/// no list at all (PRP1 alone, or PRP1 plus PRP2 pointing at the second
/// page). Beyond that, `list` is filled lazily on first use with one 8-byte
/// entry per remaining page -- one list page (4 KiB) covers
/// [`PRP_LIST_ENTRIES`] pages (2 MiB), which is why one page per command id
/// is always enough for any transfer this driver issues.
///
/// Every page is translated separately. A virtually contiguous buffer is not
/// physically contiguous here: the kernel heap is mapped by `map_memory`,
/// which allocates a frame at a time, and only `map_memory_contiguous` (what
/// the DMA allocator uses) yields a run of frames. Deriving later pages by
/// adding 4096 to the first would address unrelated frames and DMA into
/// them.
///
/// `Err(Unsupported)` means a page did not translate, which the caller
/// answers by bouncing through a `dma()` buffer.
///
/// Every page past PRP1 is counted in [`stats::PRP_PAGES`], and the ones
/// whose frame is not the first page's frame plus the page index in
/// [`stats::PRP_PAGES_DISCONTIGUOUS`]. That second counter is what makes
/// the translation observable: it is derived from the difference between
/// the translated address and the naive one, so a regression that goes
/// back to deriving addresses arithmetically drives it to zero rather than
/// leaving it unchanged.
///
/// Callers that error out, get cancelled, or otherwise never reach a
/// completion must reclaim `*list` themselves via `dma().dealloc`:
/// `DmaBuffer` has no `Drop`.
pub fn build_prp(
    vaddr: VirtAddr,
    len: usize,
    list: &mut Option<DmaBuffer>,
) -> Result<(u64, u64), NvmeError> {
    fn translate(vaddr: VirtAddr) -> Option<PhysAddr> {
        // Acquired and dropped per page, as AHCI's own scatter-gather walk
        // does, so the mapper is never held across the rest of the submit.
        match memory_mapper().translate(vaddr) {
            TranslateResult::Mapped { frame, offset, .. } => Some(frame.start_address() + offset),
            _ => None,
        }
    }

    let first = translate(vaddr).ok_or(NvmeError::Unsupported)?;
    let page_offset = (first.as_u64() & 0xFFF) as usize;
    let first_page_bytes = 4096 - page_offset;
    if len <= first_page_bytes {
        return Ok((first.as_u64(), 0));
    }

    // The address each page would have had if it were derived by adding
    // 4096 to the first, which is the bug this counting exists to expose.
    let naive = |i: u64| (first.as_u64() & !0xFFF) + (i + 1) * 4096;
    let count_page = |i: u64, phys: PhysAddr| {
        stats::bump(&stats::PRP_PAGES, 1);
        if phys.as_u64() != naive(i) {
            stats::bump(&stats::PRP_PAGES_DISCONTIGUOUS, 1);
        }
    };

    let second_virt = VirtAddr::new((vaddr.as_u64() & !0xFFF) + 4096);
    let second = translate(second_virt).ok_or(NvmeError::Unsupported)?;
    debug_assert_eq!(second.as_u64() & 0xFFF, 0, "PRP2 must be page-aligned");
    count_page(0, second);
    let remaining = len - first_page_bytes;
    if remaining <= 4096 {
        return Ok((first.as_u64(), second.as_u64()));
    }

    let num_pages = remaining.div_ceil(4096);
    if num_pages > PRP_LIST_ENTRIES {
        return Err(NvmeError::Unsupported);
    }
    let list_buf = match list {
        Some(buf) => buf,
        None => {
            let buf = dma()
                .allocate_sized_uninit(4096)
                .map_err(NvmeError::DmaError)?;
            list.insert(buf)
        }
    };
    let entries = list_buf.as_ptr().cast::<u64>();
    for i in 0..num_pages {
        let page_virt = VirtAddr::new(second_virt.as_u64() + (i as u64) * 4096);
        let page_phys = translate(page_virt).ok_or(NvmeError::Unsupported)?;
        // Page 0 of the list is the same page PRP2 would have carried, and
        // was counted above before the list turned out to be needed.
        if i > 0 {
            count_page(i as u64, page_phys);
        }
        debug_assert_eq!(
            page_phys.as_u64() & 0xFFF,
            0,
            "PRP list entry must be page-aligned"
        );
        unsafe {
            ptr::write_volatile(entries.add(i), page_phys.as_u64());
        }
    }
    Ok((first.as_u64(), list_buf.phys_addr().as_u64()))
}
