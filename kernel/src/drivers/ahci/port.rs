use core::{
    ptr,
    sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, Ordering},
    time::Duration,
};

use alloc::{
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};

use x86_64::structures::paging::mapper::TranslateResult;
use x86_64::{PhysAddr, VirtAddr};

use spin::Once;

use crate::{
    debug::lock_order::{RANK_AHCI_LEGACY, RANK_AHCI_MMIO, RANK_AHCI_SLOT},
    drivers::{
        ahci::{
            AhciError, DeviceType,
            cancel_op::{
                AhciNcqOp, AhciSlotOp, SLOT_CANCELLED, SLOT_COMPLETED, SLOT_PENDING, SlotCompletion,
            },
            fis::FisRegH2D,
            structures::{
                CMD_HEADER_ATAPI, CMD_HEADER_WRITE, CommandHeader, CommandTable,
                DeviceIdentifyInfo, HbaFis, HbaPort, MAX_PRDT_ENTRIES, PORT_CMD_CR, PORT_CMD_FR,
                PORT_CMD_FRE, PORT_CMD_ST, PORT_IS_TFES, PrdtEntry, ScsiInquiry, ScsiRead10,
                ScsiReadCapacity10,
            },
        },
        dma::{DmaBuffer, DmaRegion, dma},
    },
    log,
    memory::mapper::memory_mapper,
    ranked_lock,
    thread::{
        cancel::ArcCancellableOp,
        mutex::BlockingMutex,
        scheduler::{WakePriority, sched},
        waitqueue::WaitQueue,
    },
};

const AHCI_CMD_SLOTS: usize = 32;

/// Number of 4KB pages per command slot for scatter-gather I/O.
/// 248 pages = 992KB max per command (matches CommandTable PRDT capacity).
/// With 32 NCQ slots that's ~31MB total per port.
pub const NCQ_PAGES_PER_SLOT: usize = 248;

/// Per-slot pre-allocated DMA page pool for scatter-gather I/O.
struct SlotPool {
    pages: Vec<DmaBuffer>,
    phys: Vec<PhysAddr>,
}

/// Translate a virtual buffer into a scatter-gather list of (phys_addr, byte_count) entries.
/// Returns None if any page translation fails. Merges physically contiguous entries.
/// IMPORTANT: The IrqLock on memory_mapper() is acquired and dropped per-page translation.
/// Safety: x86-64 PCIe DMA is cache-coherent; dirty WB cache lines are visible to the
/// HBA via bus snooping. Do not use on non-coherent architectures without cache flushes.
fn virt_buffer_to_sg_list(
    buf: *const u8,
    len: usize,
) -> Option<heapless::Vec<(PhysAddr, usize), MAX_PRDT_ENTRIES>> {
    if len == 0 {
        return Some(heapless::Vec::new());
    }
    // AHCI PRDT DBA must be word-aligned (2-byte). Fall back to pool path if not.
    if buf as usize % 2 != 0 {
        return None;
    }

    let mut sg = heapless::Vec::new();
    let mut remaining = len;
    let mut vaddr = VirtAddr::new(buf as u64);

    while remaining > 0 {
        // Translate this virtual address to physical. Acquire/drop lock per page.
        let phys = {
            let mapper = memory_mapper();
            match mapper.translate(vaddr) {
                TranslateResult::Mapped { frame, offset, .. } => frame.start_address() + offset,
                _ => return None,
            }
        }; // IrqLock dropped here

        // Bytes available in this page from the current offset
        let page_offset = vaddr.as_u64() as usize & 0xFFF;
        let chunk = remaining.min(4096 - page_offset);

        // Try to merge with previous entry if physically contiguous.
        // AHCI PRDT DBC is a 22-bit field with N-1 encoding: max 4 MiB per
        // entry. Without this cap, large physically-contiguous buffers would
        // silently truncate to (byte_count & 0x3FFFFF) and the HBA would
        // transfer only the low 4 MiB, leaving the tail uninitialized.
        const MAX_PRDT_ENTRY_BYTES: usize = 4 * 1024 * 1024;
        if let Some(last) = sg.last_mut() {
            let (last_phys, last_len): &mut (PhysAddr, usize) = last;
            if *last_phys + *last_len as u64 == phys && *last_len + chunk <= MAX_PRDT_ENTRY_BYTES {
                *last_len += chunk;
                remaining -= chunk;
                vaddr += chunk as u64;
                continue;
            }
        }

        // New entry
        if sg.push((phys, chunk)).is_err() {
            return None; // Exceeded MAX_PRDT_ENTRIES
        }

        remaining -= chunk;
        vaddr += chunk as u64;
    }

    Some(sg)
}

pub struct AhciPort {
    pub port_idx: usize,
    pub port_regs: *mut HbaPort,
    pub device_type: DeviceType,
    ncq_enabled: AtomicBool,
    ncq_depth: AtomicU8,
    supports_fua: AtomicBool,

    // DMA regions (immutable after init)
    command_list: DmaRegion<[CommandHeader; AHCI_CMD_SLOTS]>,
    fis_area: DmaRegion<HbaFis>,
    command_tables: [Once<DmaRegion<CommandTable>>; AHCI_CMD_SLOTS],

    // Per-slot scatter-gather page pools.
    // For NCQ: ncq_depth slots allocated. For non-NCQ ATA: 1 slot (slot 0).
    // For ATAPI: empty (ATAPI uses temporary DMA buffers, not scatter pools).
    slot_pools: Once<Vec<SlotPool>>,

    // Slot management -- atomic for lock-free NCQ slot allocation.
    free_slots: AtomicU32,

    // Brief spinlock for MMIO register writes (SACT + CI read-modify-write).
    mmio_lock: spin::Mutex<()>,

    // Per-slot waiter handles. The AHCI driver kthread reads these to wake
    // the correct I/O thread when a command completes. None = no waiter.
    //
    // Each slot holds an `Arc<AhciSlotOp>` which carries: the submitter's
    // `Weak<Thread>` (for waking), a `Weak<AhciPort>` (for cancel-path slot
    // release), and an `AtomicU8` state machine (Pending/Completed/Cancelled).
    //
    // IRQ-safety: this uses plain `spin::Mutex`, which does NOT disable
    // interrupts. Every access site runs in thread context: submitters are the
    // I/O-issuing threads (e.g. userspace via syscall), `wake_all_slot_waiters`
    // runs in the AHCI driver kthread's completion path, and `cancel()` runs
    // on the reaper kthread via `Thread::free`. The hardware IRQ handler only
    // wakes `AHCI_DRIVER_THREAD_ID`; it does NOT touch `slot_waiters` directly.
    // If that ever changes (e.g. async NCQ readahead waking directly from IRQ),
    // this MUST become `IrqSpinlock` first — otherwise a lock holder preempted
    // by an IRQ that also tries to acquire the lock will deadlock.
    slot_waiters: [spin::Mutex<Option<Arc<AhciSlotOp>>>; AHCI_CMD_SLOTS],

    // Per-slot async NCQ trackers. Populated by `submit_ncq_*` and cleared
    // by the IRQ dispatcher's `on_port_irq` when `SACT[slot]` goes 0. Same
    // IRQ-safety reasoning as `slot_waiters` above.
    ncq_waiters: [spin::Mutex<Option<Arc<AhciNcqOp>>>; AHCI_CMD_SLOTS],

    // Weak self-reference for cancel path: `AhciSlotOp::cancel` upgrades this
    // to call `release_orphaned_slot`. Set immediately after `Arc::new(port)`
    // in `ahci/mod.rs`; must not be called before that.
    weak_self: Once<Weak<AhciPort>>,

    // NCQ / non-NCQ mode exclusion (only meaningful when ncq_enabled).
    //   > 0 : count of in-flight NCQ (FPDMA) commands
    //    -1 : a legacy (non-NCQ) command is active
    //     0 : idle
    mode: AtomicI32,
    mode_waitq: WaitQueue,

    // Blocks when all NCQ slots are in use.
    slot_waitq: WaitQueue,

    // Guards restart_port so only one thread runs it after NCQ error.
    restarting: AtomicBool,

    // Bumped on every successful `restart_port`. NCQ waiters sample this
    // before submit; if it changes during their wait, COMRESET wiped SACT
    // and the slot's "cleared" bit no longer means success -- it means
    // the command was killed. Without this guard the waiter would return
    // Ok with a stale (uninitialized) buffer.
    reset_generation: AtomicU32,

    // Serializes legacy (non-NCQ) commands among each other.
    legacy_lock: BlockingMutex<()>,
}

unsafe impl Send for AhciPort {}
// Safety: all mutable state is behind atomics, spin::Mutex, BlockingMutex, or WaitQueue.
// Per-slot DMA regions (command_tables, slot_pools) are accessed only by the thread
// that owns the slot (guaranteed by atomic free_slots allocation). The command_list
// DMA region is written per-slot (disjoint offsets) and is NO_CACHE mapped.
unsafe impl Sync for AhciPort {}

// ---------------------------------------------------------------------------
// Construction and initialization
// ---------------------------------------------------------------------------

impl AhciPort {
    pub fn new(
        port_idx: usize,
        port_regs: *mut HbaPort,
        device_type: DeviceType,
    ) -> Result<Self, AhciError> {
        log!("Initializing AHCI port {}", port_idx);

        Self::stop_port(port_regs)?;

        let command_list = dma().allocate()?;
        let fis_area = dma().allocate()?;

        unsafe {
            ptr::write_volatile(
                &raw mut (*port_regs).clb,
                command_list.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*port_regs).clbu,
                (command_list.phys_addr().as_u64() >> 32) as u32,
            );
            ptr::write_volatile(
                &raw mut (*port_regs).fb,
                fis_area.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*port_regs).fbu,
                (fis_area.phys_addr().as_u64() >> 32) as u32,
            );

            // Clear interrupt status
            ptr::write_volatile(&raw mut (*port_regs).is, 0xFFFFFFFF);

            // Enable FIS receive + start port
            let mut cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            cmd |= PORT_CMD_FRE;
            ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);
            cmd |= PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);
        }

        unsafe {
            // Enable interrupts: DHRS, DSS, SDBS (NCQ completion), DPS, TFES.
            let ie = (1 << 0)   // DHRS  - Device to Host Register FIS
                   | (1 << 2)   // DSS   - DMA Setup FIS
                   | (1 << 3)   // SDBS  - Set Device Bits FIS (NCQ completion)
                   | (1 << 5)   // DPS   - Descriptor Processed
                   | (1 << 30); // TFES  - Task File Error
            ptr::write_volatile(&raw mut (*port_regs).ie, ie);
        }

        // Pre-allocate command table for slot 0 (needed for IDENTIFY during init).
        // Allocate outside `call_once` so we can propagate failure via `?`.
        let command_tables: [Once<DmaRegion<CommandTable>>; AHCI_CMD_SLOTS] =
            [const { Once::new() }; AHCI_CMD_SLOTS];
        let slot0_table = dma().allocate()?;
        command_tables[0].call_once(|| slot0_table);

        log!("Port {} initialized successfully", port_idx);

        Ok(Self {
            port_idx,
            port_regs,
            device_type,
            ncq_enabled: AtomicBool::new(false),
            ncq_depth: AtomicU8::new(0),
            supports_fua: AtomicBool::new(false),
            command_list,
            fis_area,
            command_tables,
            slot_pools: Once::new(),
            free_slots: AtomicU32::new(1), // Only slot 0 available until init_io_pools
            mmio_lock: spin::Mutex::new(()),
            slot_waiters: [const { spin::Mutex::new(None) }; AHCI_CMD_SLOTS],
            ncq_waiters: [const { spin::Mutex::new(None) }; AHCI_CMD_SLOTS],
            weak_self: Once::new(),
            mode: AtomicI32::new(0),
            mode_waitq: WaitQueue::new(),
            slot_waitq: WaitQueue::new(),
            restarting: AtomicBool::new(false),
            reset_generation: AtomicU32::new(0),
            legacy_lock: BlockingMutex::new(()),
        })
    }

    /// Post-identify initialization: allocate per-slot DMA pools and command tables.
    ///
    /// `ncq_depth`: effective NCQ queue depth (min of HBA and device). 0 if no NCQ.
    /// Must be called exactly once, after `set_weak_self`.
    pub fn init_io_pools(&self, ncq_depth: u8, supports_fua: bool) -> Result<(), AhciError> {
        let use_ncq = ncq_depth > 0 && self.device_type == DeviceType::Ata;
        self.ncq_depth
            .store(if use_ncq { ncq_depth } else { 0 }, Ordering::Release);
        self.ncq_enabled.store(use_ncq, Ordering::Release);
        self.supports_fua.store(supports_fua, Ordering::Release);

        let num_slots = if use_ncq {
            ncq_depth as usize
        } else if self.device_type == DeviceType::Ata {
            1 // Non-NCQ ATA: 1 slot for scatter-gather reads
        } else {
            0 // ATAPI: no scatter-gather pools
        };

        // Pre-allocate command tables for slots 1..num_slots (slot 0 done in new()).
        // Build the slot pools Vec and store it via Once.
        for slot in 1..num_slots {
            let table = dma().allocate()?;
            self.command_tables[slot].call_once(|| table);
        }

        let mut pools = Vec::with_capacity(num_slots);
        for _ in 0..num_slots {
            let mut pages = Vec::with_capacity(NCQ_PAGES_PER_SLOT);
            let mut phys = Vec::with_capacity(NCQ_PAGES_PER_SLOT);
            for _ in 0..NCQ_PAGES_PER_SLOT {
                let buf = dma().allocate_sized(4096)?;
                phys.push(buf.phys_addr());
                pages.push(buf);
            }
            pools.push(SlotPool { pages, phys });
        }
        self.slot_pools.call_once(|| pools);

        // Set free_slots to include all usable slots.
        let mask = if num_slots > 0 {
            if num_slots >= 32 {
                0xFFFF_FFFF
            } else {
                (1u32 << num_slots) - 1
            }
        } else {
            // ATAPI: slot 0 only (for control commands)
            1
        };
        self.free_slots.store(mask, Ordering::Release);

        if use_ncq {
            log!(
                "Port {}: NCQ enabled, depth {}, {}MB DMA pools",
                self.port_idx,
                ncq_depth,
                (num_slots * NCQ_PAGES_PER_SLOT * 4096) / (1024 * 1024)
            );
        } else if self.device_type == DeviceType::Ata {
            log!(
                "Port {}: legacy DMA, 1 slot, {}KB pool",
                self.port_idx,
                NCQ_PAGES_PER_SLOT * 4
            );
        }

        Ok(())
    }

    fn stop_port(port_regs: *mut HbaPort) -> Result<(), AhciError> {
        unsafe {
            let mut cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            if cmd & PORT_CMD_ST != 0 {
                cmd &= !PORT_CMD_ST;
                ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

                let start = crate::timer::Instant::now();
                while ptr::read_volatile(&raw const (*port_regs).cmd) & PORT_CMD_CR != 0 {
                    if start.elapsed().as_millis() > 500 {
                        return Err(AhciError::CommandTimeout);
                    }
                    sched().thread_sleep(Duration::from_millis(1));
                }
            }

            cmd = ptr::read_volatile(&raw const (*port_regs).cmd);
            if cmd & PORT_CMD_FRE != 0 {
                cmd &= !PORT_CMD_FRE;
                ptr::write_volatile(&raw mut (*port_regs).cmd, cmd);

                let start = crate::timer::Instant::now();
                while ptr::read_volatile(&raw const (*port_regs).cmd) & PORT_CMD_FR != 0 {
                    if start.elapsed().as_millis() > 500 {
                        return Err(AhciError::CommandTimeout);
                    }
                    sched().thread_sleep(Duration::from_millis(1));
                }
            }
        }
        Ok(())
    }

    /// Restart port after error recovery. Re-programs CLB/FB from existing DMA regions.
    ///
    /// Stopping ST/FRE does NOT clear `PxSACT` or `PxCI`: both are `R/W1S` per
    /// the AHCI 1.3.1 spec, cleared only by an SDB FIS, COMRESET, or HBA
    /// reset. If we recycle a slot whose prior submission left SACT.N=1, the
    /// next OR-write of the same bit is a hardware no-op and the drive never
    /// retriggers. We issue a COMRESET whenever residual SACT/CI bits are
    /// observed after the stop, which is exactly the NCQ-timeout case.
    fn restart_port(&self) -> Result<(), AhciError> {
        Self::stop_port(self.port_regs)?;
        unsafe {
            let residual_sact = ptr::read_volatile(&raw const (*self.port_regs).sact);
            let residual_ci = ptr::read_volatile(&raw const (*self.port_regs).ci);
            if residual_sact != 0 || residual_ci != 0 {
                log!(
                    "AHCI port {}: residual SACT={:#x} CI={:#x} after stop, issuing COMRESET",
                    self.port_idx,
                    residual_sact,
                    residual_ci
                );
                self.comreset()?;
            }

            // Clear error state
            ptr::write_volatile(&raw mut (*self.port_regs).serr, 0xFFFFFFFF);
            ptr::write_volatile(&raw mut (*self.port_regs).is, 0xFFFFFFFF);

            // Re-program CLB/FB (already allocated)
            ptr::write_volatile(
                &raw mut (*self.port_regs).clb,
                self.command_list.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*self.port_regs).clbu,
                (self.command_list.phys_addr().as_u64() >> 32) as u32,
            );
            ptr::write_volatile(
                &raw mut (*self.port_regs).fb,
                self.fis_area.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*self.port_regs).fbu,
                (self.fis_area.phys_addr().as_u64() >> 32) as u32,
            );

            // Enable FIS receive + start port
            let mut cmd = ptr::read_volatile(&raw const (*self.port_regs).cmd);
            cmd |= PORT_CMD_FRE;
            ptr::write_volatile(&raw mut (*self.port_regs).cmd, cmd);
            cmd |= PORT_CMD_ST;
            ptr::write_volatile(&raw mut (*self.port_regs).cmd, cmd);
        }
        // Publish: any NCQ waiter that captured the prior generation now
        // sees a different value and must return IoError rather than
        // interpreting its cleared SACT bit as success.
        self.reset_generation.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Issue a COMRESET on the port to clear `PxSACT` / `PxCI` and force the
    /// drive to re-establish the link. Caller must stop the port (ST=0, FRE=0)
    /// first. Per AHCI 1.3.1 section 10.4.2:
    ///
    /// 1. PxSCTL.DET = 1 (initiate COMRESET).
    /// 2. Wait at least 1 ms.
    /// 3. PxSCTL.DET = 0 (release).
    /// 4. Wait for PxSSTS.DET = 3 (device present + PHY ready).
    ///
    /// Returns `CommandTimeout` if the drive fails to re-establish within 1 s.
    fn comreset(&self) -> Result<(), AhciError> {
        unsafe {
            let mut sctl = ptr::read_volatile(&raw const (*self.port_regs).sctl);
            sctl = (sctl & !0xF) | 0x1;
            ptr::write_volatile(&raw mut (*self.port_regs).sctl, sctl);
            sched().thread_sleep(Duration::from_millis(2));
            sctl &= !0xF;
            ptr::write_volatile(&raw mut (*self.port_regs).sctl, sctl);

            let start = crate::timer::Instant::now();
            loop {
                let ssts = ptr::read_volatile(&raw const (*self.port_regs).ssts);
                if ssts & 0xF == 0x3 {
                    break;
                }
                if start.elapsed().as_millis() > 1000 {
                    log!(
                        "AHCI port {}: COMRESET timed out (SSTS={:#x})",
                        self.port_idx,
                        ssts
                    );
                    return Err(AhciError::CommandTimeout);
                }
                sched().thread_sleep(Duration::from_millis(1));
            }

            let sact = ptr::read_volatile(&raw const (*self.port_regs).sact);
            let ci = ptr::read_volatile(&raw const (*self.port_regs).ci);
            debug_assert_eq!(
                sact, 0,
                "COMRESET did not clear PxSACT on port {} (SACT={:#x})",
                self.port_idx, sact,
            );
            debug_assert_eq!(
                ci, 0,
                "COMRESET did not clear PxCI on port {} (CI={:#x})",
                self.port_idx, ci,
            );
        }
        Ok(())
    }

    pub fn set_device_type(&mut self, device_type: DeviceType) {
        self.device_type = device_type;
    }

    /// Store the weak self-reference. Called immediately after `Arc::new(port)`
    /// in `ahci/mod.rs` before the port enters `DIRECT_PORTS`.
    pub fn set_weak_self(&self, weak: Weak<AhciPort>) {
        self.weak_self.call_once(|| weak);
    }

    /// Upgrade `weak_self` to a strong `Arc<AhciPort>`.
    ///
    /// # Panics
    /// - If called before `set_weak_self`.
    /// - If the port has been dropped (should not happen when called from
    ///   within a method on `&self`).
    #[allow(dead_code)]
    pub fn self_arc(&self) -> Arc<AhciPort> {
        self.weak_self
            .get()
            .expect("AhciPort::self_arc called before set_weak_self")
            .upgrade()
            .expect("AhciPort dropped while self_arc() caller held &self")
    }

    /// Release an orphaned slot: clear the waiter entry and return the slot to
    /// the free pool.
    ///
    /// Called from two places:
    /// 1. `AhciSlotOp::cancel` — submitter died, cancel path won the CAS.
    /// 2. `wake_all_slot_waiters` — normal completion raced with a dead/dying
    ///    thread; completion won the CAS but there is no one to wake.
    ///
    /// Must only be called once per slot lifetime (the CAS in `cancel` /
    /// `wake_all_slot_waiters` guarantees this). Runs in thread context.
    #[inline]
    pub fn release_orphaned_slot(&self, slot: usize) {
        **ranked_lock!(
            RANK_AHCI_SLOT,
            "AhciPort.slot_waiters",
            self.slot_waiters[slot]
        ) = None;
        self.free_slot(slot);
    }

    /// Conditionally release a slot, only if `op` is still the stored waiter.
    ///
    /// Used by `AhciSlotOp::cancel` in the `Err(SLOT_COMPLETED)` branch:
    /// the IRQ path already set Completed and chose to wake the thread (not
    /// call `release_orphaned_slot`), but the thread was already dying. The
    /// cancel path then checks — if our Arc is still in `slot_waiters`, the
    /// submitter never cleared it (because it never woke up), so we free it.
    /// If another op already occupies the slot, do nothing.
    ///
    /// Uses raw pointer equality on `AhciSlotOp` (same allocation) rather
    /// than `Arc::ptr_eq` because the cancel path has `&AhciSlotOp`, not an
    /// `Arc<AhciSlotOp>`.
    pub fn maybe_release_slot(&self, slot: usize, op: &AhciSlotOp) {
        let op_ptr = op as *const AhciSlotOp;
        let still_there = {
            let mut g = ranked_lock!(
                RANK_AHCI_SLOT,
                "AhciPort.slot_waiters",
                self.slot_waiters[slot]
            );
            let is_ours = g
                .as_ref()
                .map(|stored| Arc::as_ptr(stored) == op_ptr)
                .unwrap_or(false);
            if is_ours {
                **g = None;
                true
            } else {
                false
            }
        };
        if still_there {
            self.free_slot(slot);
        }
    }

    // ---- Interior-mutable field accessors ----------------------------------

    fn ncq_enabled(&self) -> bool {
        self.ncq_enabled.load(Ordering::Acquire)
    }

    fn supports_fua(&self) -> bool {
        self.supports_fua.load(Ordering::Acquire)
    }

    fn command_table(&self, slot: usize) -> &DmaRegion<CommandTable> {
        self.command_tables[slot]
            .get()
            .expect("command_table not initialized for slot")
    }

    fn slot_pool(&self, slot: usize) -> &SlotPool {
        &self.slot_pools.get().expect("slot_pools not initialized")[slot]
    }

    fn slot_pools_len(&self) -> usize {
        self.slot_pools.get().map(|v| v.len()).unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// Slot management (lock-free via atomics)
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Allocate a free command slot. Returns None if all slots are in use.
    fn allocate_slot(&self) -> Option<usize> {
        loop {
            let slots = self.free_slots.load(Ordering::Acquire);
            if slots == 0 {
                return None;
            }
            let slot = slots.trailing_zeros() as usize;
            if self
                .free_slots
                .compare_exchange_weak(
                    slots,
                    slots & !(1 << slot),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                return Some(slot);
            }
        }
    }

    /// Allocate a slot, blocking if all are in use.
    fn allocate_slot_blocking(&self) -> usize {
        loop {
            if let Some(slot) = self.allocate_slot() {
                return slot;
            }
            self.slot_waitq
                .wait_until(|| self.free_slots.load(Ordering::Acquire) != 0);
        }
    }

    /// Return a slot to the free pool.
    fn free_slot(&self, slot: usize) {
        self.free_slots.fetch_or(1 << slot, Ordering::Release);
        self.slot_waitq.wake_one();
    }

    /// Allocate a slot (non-blocking), create an `AhciSlotOp`, register it
    /// with the submitter's `owned_ops`, run `f`, then clean up.
    ///
    /// Used by non-NCQ / legacy paths where no slot available is an error.
    /// ATAPI commands use try semantics (matching pre-Foundation-#2 behavior:
    /// `execute_atapi_command` always used `allocate_slot`, never blocking).
    ///
    /// Returns `Err(AhciError::PortNotReady)` immediately if no slot is free.
    /// If `f` returns `Err`, the slot is still cleaned up.
    fn with_slot_try<R>(
        &self,
        f: impl FnOnce(usize, &Arc<AhciSlotOp>) -> Result<R, AhciError>,
    ) -> Result<R, AhciError> {
        let slot = self.allocate_slot().ok_or(AhciError::PortNotReady)?;

        let port_weak = self
            .weak_self
            .get()
            .cloned()
            .expect("AhciPort::set_weak_self must be called before any command submission");
        let waiter = sched().current_thread_weak().unwrap_or_default();
        let op = Arc::new(AhciSlotOp::new(port_weak, slot, waiter));
        **ranked_lock!(
            RANK_AHCI_SLOT,
            "AhciPort.slot_waiters",
            self.slot_waiters[slot]
        ) = Some(Arc::clone(&op));

        // Register with owned_ops BEFORE parking inside f().
        let current = sched().current_thread();
        let push_ok = if let Some(ref t) = current {
            t.owned_ops_push(Arc::clone(&op) as ArcCancellableOp)
                .is_ok()
        } else {
            false
        };
        if !push_ok {
            log!(
                "AHCI port {}: owned_ops full for slot {}; cancel hookup skipped",
                self.port_idx,
                slot
            );
        }

        let result = f(slot, &op);

        // Drive the state machine on success. Non-NCQ / polling paths
        // complete synchronously inside `f` without touching state, so the
        // CAS here catches them up. NCQ paths (if any reach here — most
        // don't via _try) would already be at COMPLETED and the CAS is a
        // no-op. See `with_slot_blocking` for the full rationale.
        if result.is_ok() {
            let _ = op.state.compare_exchange(
                SLOT_PENDING,
                SLOT_COMPLETED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            debug_assert_ne!(
                op.state.load(Ordering::Acquire),
                SLOT_CANCELLED,
                "AHCI slot {} cancelled on Ok path",
                slot
            );
        }

        // Deregister from owned_ops.
        if push_ok {
            if let Some(ref t) = current {
                t.owned_ops_remove(Arc::as_ptr(&op) as *const ());
            }
        }

        // Idempotent cleanup: only free the slot if it is still ours.
        // The ptr_eq guard is load-bearing: a concurrent release_orphaned_slot
        // (from wake_all_slot_waiters seeing a Dying thread) may have already
        // cleared slot_waiters and freed the slot; without this check we'd double-free.
        let still_ours = {
            let mut g = ranked_lock!(
                RANK_AHCI_SLOT,
                "AhciPort.slot_waiters",
                self.slot_waiters[slot]
            );
            if g.as_ref().map(|o| Arc::ptr_eq(o, &op)).unwrap_or(false) {
                **g = None;
                true
            } else {
                false
            }
        };
        if still_ours {
            self.free_slot(slot);
        }

        result
    }
}

// ---------------------------------------------------------------------------
// NCQ / non-NCQ mode exclusion
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Enter NCQ mode (increment in-flight counter). Blocks if legacy mode is active.
    fn enter_ncq_mode(&self) {
        loop {
            let current = self.mode.load(Ordering::Acquire);
            if current >= 0 {
                if self
                    .mode
                    .compare_exchange_weak(
                        current,
                        current + 1,
                        Ordering::AcqRel,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return;
                }
                continue;
            }
            // Legacy mode active (-1), wait for it to finish.
            self.mode_waitq
                .wait_until(|| self.mode.load(Ordering::Acquire) >= 0);
        }
    }

    /// Exit NCQ mode (decrement in-flight counter). Wakes legacy waiters if last.
    fn exit_ncq_mode(&self) {
        let prev = self.mode.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(prev > 0);
        if prev == 1 {
            // Last NCQ command finished, wake any thread waiting for legacy mode.
            self.mode_waitq.wake_all();
        }
    }

    /// Enter legacy mode (set mode to -1). Blocks until all NCQ commands drain.
    fn enter_legacy_mode(&self) {
        loop {
            if self
                .mode
                .compare_exchange_weak(0, -1, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            // NCQ or other legacy active, wait.
            self.mode_waitq
                .wait_until(|| self.mode.load(Ordering::Acquire) == 0);
        }
    }

    /// Exit legacy mode. Wakes NCQ waiters.
    fn exit_legacy_mode(&self) {
        self.mode.store(0, Ordering::Release);
        self.mode_waitq.wake_all();
    }
}

// ---------------------------------------------------------------------------
// Command setup
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Set up a command table with FIS and a single PRDT entry (for IDENTIFY, FLUSH, etc.).
    fn setup_command_table(
        &self,
        slot: usize,
        fis: &FisRegH2D,
        buffer_addr: PhysAddr,
        buffer_size: usize,
    ) -> Result<(), AhciError> {
        let table_ref = self.command_table(slot);

        let has_data = buffer_size > 0;

        unsafe {
            let table = table_ref.get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            let fis_bytes = bytemuck::bytes_of(fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            if has_data {
                table.prdt[0] = PrdtEntry {
                    dba: buffer_addr.as_u64() as u32,
                    dbau: (buffer_addr.as_u64() >> 32) as u32,
                    reserved: 0,
                    dbc: (buffer_size - 1) as u32,
                };
            }
        }

        Ok(())
    }

    /// Set up a command table with scatter-gather PRDT entries from the per-slot pool.
    fn setup_scatter_command(
        &self,
        slot: usize,
        fis: &FisRegH2D,
        total_bytes: usize,
    ) -> Result<(), AhciError> {
        let num_entries = total_bytes.div_ceil(4096);
        debug_assert!(num_entries <= NCQ_PAGES_PER_SLOT);
        debug_assert!(slot < self.slot_pools_len(), "slot {slot} has no pool");

        let table_ref = self.command_table(slot);

        unsafe {
            let table = table_ref.get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            let fis_bytes = bytemuck::bytes_of(fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            let pool = self.slot_pool(slot);
            let mut remaining = total_bytes;
            for i in 0..num_entries {
                let phys = pool.phys[i];
                let chunk = remaining.min(4096);
                table.prdt[i] = PrdtEntry {
                    dba: phys.as_u64() as u32,
                    dbau: (phys.as_u64() >> 32) as u32,
                    reserved: 0,
                    dbc: (chunk as u32) - 1,
                };
                remaining -= chunk;
            }
        }

        Ok(())
    }

    /// Set up a command table with PRDT entries from a pre-built scatter-gather list.
    /// Used for zero-copy DMA where PRDT points directly to the caller's buffer pages.
    fn setup_scatter_direct(
        &self,
        slot: usize,
        fis: &FisRegH2D,
        sg_list: &[(PhysAddr, usize)],
    ) -> Result<(), AhciError> {
        debug_assert!(sg_list.len() <= MAX_PRDT_ENTRIES);

        let table_ref = self.command_table(slot);

        unsafe {
            let table = table_ref.get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            let fis_bytes = bytemuck::bytes_of(fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            for (i, &(phys, byte_count)) in sg_list.iter().enumerate() {
                debug_assert!(phys.as_u64() % 2 == 0, "PRDT DBA must be word-aligned");
                debug_assert!(byte_count > 0, "zero-length PRDT entry");
                debug_assert!(
                    byte_count <= 4 * 1024 * 1024,
                    "PRDT entry exceeds 4 MiB DBC limit: {byte_count}"
                );
                if byte_count == 0 || byte_count > 4 * 1024 * 1024 {
                    return Err(AhciError::IoError);
                }
                table.prdt[i] = PrdtEntry {
                    dba: phys.as_u64() as u32,
                    dbau: (phys.as_u64() >> 32) as u32,
                    reserved: 0,
                    dbc: (byte_count as u32) - 1,
                };
            }
        }

        Ok(())
    }

    /// Set up an ATAPI command table with PACKET FIS and SCSI CDB.
    fn setup_atapi_command_table(
        &self,
        slot: usize,
        scsi_cmd: &[u8],
        buffer_addr: PhysAddr,
        buffer_size: usize,
    ) -> Result<(), AhciError> {
        if scsi_cmd.len() > 16 {
            return Err(AhciError::IoError);
        }

        let table_ref = self.command_table(slot);

        unsafe {
            let table = table_ref.get();
            table.write(core::mem::zeroed());
            let table = &mut *table;

            let packet_fis = FisRegH2D::new_atapi_packet(buffer_size as u16);
            let fis_bytes = bytemuck::bytes_of(&packet_fis);
            table.cfis[..fis_bytes.len()].copy_from_slice(fis_bytes);

            table.acmd[..scsi_cmd.len()].copy_from_slice(scsi_cmd);

            if buffer_size > 0 {
                table.prdt[0] = PrdtEntry {
                    dba: buffer_addr.as_u64() as u32,
                    dbau: (buffer_addr.as_u64() >> 32) as u32,
                    reserved: 0,
                    dbc: (buffer_size - 1) as u32,
                };
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Command issue and completion
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Write command header and issue a non-NCQ command (CI only).
    fn issue_command(&self, slot: usize, flags: u16, prdtl: u16) -> Result<(), AhciError> {
        let table_ref = self.command_table(slot);

        // Write command header via raw pointer to avoid &mut aliasing over the full array.
        unsafe {
            let header = &raw mut (*self.command_list.get())[slot];
            ptr::write_volatile(&raw mut (*header).flags, 5 | flags);
            ptr::write_volatile(&raw mut (*header).prdtl, prdtl);
            ptr::write_volatile(&raw mut (*header).prdbc, 0);
            ptr::write_volatile(
                &raw mut (*header).ctba,
                table_ref.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*header).ctbau,
                (table_ref.phys_addr().as_u64() >> 32) as u32,
            );
            ptr::write_volatile(&raw mut (*header).reserved, [0; 4]);
        }

        // Issue: write only the slot bit. CI is W1S per AHCI 1.3.1 -- writes
        // of 0 are ignored and writes of 1 set the bit, so a direct write of
        // `1 << slot` is both spec-correct and avoids the prior RMW's stale-
        // read window. mmio_lock is retained to keep CI/SACT writes on this
        // port ordered with respect to each other.
        let _lock = ranked_lock!(RANK_AHCI_MMIO, "AhciPort.mmio_lock", self.mmio_lock);
        unsafe {
            ptr::write_volatile(&raw mut (*self.port_regs).ci, 1u32 << slot);
        }

        Ok(())
    }

    /// Write command header and issue an NCQ (FPDMA) command (SACT then CI).
    fn issue_ncq_command(&self, slot: usize, flags: u16, prdtl: u16) -> Result<(), AhciError> {
        let table_ref = self.command_table(slot);

        unsafe {
            let header = &raw mut (*self.command_list.get())[slot];
            ptr::write_volatile(&raw mut (*header).flags, 5 | flags);
            ptr::write_volatile(&raw mut (*header).prdtl, prdtl);
            ptr::write_volatile(&raw mut (*header).prdbc, 0);
            ptr::write_volatile(
                &raw mut (*header).ctba,
                table_ref.phys_addr().as_u64() as u32,
            );
            ptr::write_volatile(
                &raw mut (*header).ctbau,
                (table_ref.phys_addr().as_u64() >> 32) as u32,
            );
            ptr::write_volatile(&raw mut (*header).reserved, [0; 4]);
        }

        // Issue: SACT MUST be written before CI for NCQ commands. Both
        // registers are W1S per AHCI 1.3.1 -- writing only the new bit is
        // spec-correct and skips the prior RMW's stale-read window.
        let _lock = ranked_lock!(RANK_AHCI_MMIO, "AhciPort.mmio_lock", self.mmio_lock);
        unsafe {
            ptr::write_volatile(&raw mut (*self.port_regs).sact, 1u32 << slot);
            ptr::write_volatile(&raw mut (*self.port_regs).ci, 1u32 << slot);
        }

        Ok(())
    }

    /// Wait for a non-NCQ command to complete (CI bit clears).
    fn wait_for_completion(&self, slot: usize, timeout: Duration) -> Result<(), AhciError> {
        let start = crate::timer::Instant::now();
        let port_regs = self.port_regs;

        loop {
            let ci = unsafe { ptr::read_volatile(&raw const (*port_regs).ci) };
            if ci & (1 << slot) == 0 {
                return Ok(());
            }

            let is = unsafe { ptr::read_volatile(&raw const (*port_regs).is) };
            if is & PORT_IS_TFES != 0 {
                // Don't clear port IS here; the dispatch thread handles it.
                let tfd = unsafe { ptr::read_volatile(&raw const (*port_regs).tfd) };
                log!(
                    "AHCI port {}: Command error - Status: {:#x}, Error: {:#x}",
                    self.port_idx,
                    tfd & 0xFF,
                    (tfd >> 8) & 0xFF
                );
                return Err(AhciError::IoError);
            }

            if start.elapsed() >= timeout {
                log!(
                    "AHCI port {}: Command timeout on slot {}",
                    self.port_idx,
                    slot
                );
                return Err(AhciError::CommandTimeout);
            }

            sched().thread_park_while(|| {
                let ci = unsafe { ptr::read_volatile(&raw const (*port_regs).ci) };
                let is = unsafe { ptr::read_volatile(&raw const (*port_regs).is) };
                ci & (1 << slot) != 0 && is & PORT_IS_TFES == 0 && start.elapsed() < timeout
            });
        }
    }
}

// ---------------------------------------------------------------------------
// NCQ (FPDMA) async submit/complete
//
// Submission allocates a slot, registers an `AhciNcqOp` in `ncq_waiters` plus
// the submitter's `owned_ops`, sets up the PRDT, issues the command, and
// returns the linked `BlockIoHandle`. The caller parks on `handle.wait()`.
//
// Completion is dispatcher-driven: `on_port_irq` walks `ncq_waiters` on every
// IRQ pass and, for each slot whose `SACT` bit has cleared, runs the post-
// completion copy (pool-path reads only), calls `handle.complete()`, and
// frees the slot.
//
// TFES and COMRESET error recovery also live in the dispatcher: every
// in-flight op is failed with `Io`, then `restart_port` bumps
// `reset_generation` so any stale-SACT race is caught at the
// `complete_ncq_slot` start-gen check.
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Allocate + install an `AhciNcqOp` for slot N. Returns the slot number
    /// (blocks if all slots are in use) and the Arc-wrapped op.
    fn install_ncq_op(
        self: &Arc<Self>,
        handle: Arc<BlockIoHandle>,
        buffer: BlockBuffer,
        completion: SlotCompletion,
        start_gen: u32,
    ) -> (usize, Arc<AhciNcqOp>) {
        let slot = self.allocate_slot_blocking();
        let weak_port = self
            .weak_self
            .get()
            .cloned()
            .expect("AhciPort::set_weak_self not called before submit");
        let submitter = sched().current_thread_weak().unwrap_or_default();
        let op = Arc::new(AhciNcqOp::new(
            weak_port, slot, submitter, start_gen, handle, buffer, completion,
        ));
        **ranked_lock!(
            RANK_AHCI_SLOT,
            "AhciPort.ncq_waiters",
            self.ncq_waiters[slot]
        ) = Some(Arc::clone(&op));

        if let Some(t) = sched().current_thread() {
            if t.owned_ops_push(Arc::clone(&op) as ArcCancellableOp)
                .is_err()
            {
                log!(
                    "AHCI port {}: owned_ops full for NCQ slot {}; cancel hookup skipped",
                    self.port_idx,
                    slot
                );
            }
        }
        (slot, op)
    }

    /// Tear down an `AhciNcqOp` that never reached issue (setup error).
    /// Idempotent: clears `ncq_waiters[slot]` only if our Arc is still there.
    fn unwind_ncq_op(&self, slot: usize, op: &Arc<AhciNcqOp>) {
        let _ = op.state.compare_exchange(
            SLOT_PENDING,
            SLOT_CANCELLED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if let Some(t) = sched().current_thread() {
            t.owned_ops_remove(Arc::as_ptr(op) as *const ());
        }
        let still_ours = {
            let mut g = ranked_lock!(
                RANK_AHCI_SLOT,
                "AhciPort.ncq_waiters",
                self.ncq_waiters[slot]
            );
            if g.as_ref().map(|o| Arc::ptr_eq(o, op)).unwrap_or(false) {
                **g = None;
                true
            } else {
                false
            }
        };
        if still_ours {
            self.free_slot(slot);
        }
    }

    /// Async NCQ read. Returns a `BlockIoHandle` that completes when the IRQ
    /// dispatcher observes `SACT[slot]` cleared. On pool-path reads the
    /// dispatcher performs the pool→buffer copy before signalling completion.
    pub fn submit_ncq_read(
        self: &Arc<Self>,
        lba: u64,
        sectors: u16,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let expected_size = sectors as usize * 512;
        if sectors == 0 || buffer.len() < expected_size {
            return Err(BlockError::InvalidArg);
        }
        let num_pages = expected_size.div_ceil(4096);
        if num_pages > NCQ_PAGES_PER_SLOT {
            return Err(BlockError::InvalidArg);
        }

        self.enter_ncq_mode();

        let handle = BlockIoHandle::pending();
        let start_gen = self.reset_generation.load(Ordering::Acquire);

        let sg = virt_buffer_to_sg_list(buffer.as_ptr(), expected_size);
        let completion = if sg.is_some() {
            SlotCompletion::Direct
        } else {
            SlotCompletion::PoolRead {
                num_pages,
                expected_size,
            }
        };

        let (slot, op) = self.install_ncq_op(handle.clone(), buffer, completion, start_gen);

        let fis = FisRegH2D::new_read_fpdma_queued(lba, sectors, slot as u8);
        let setup = if let Some(ref sg_list) = sg {
            self.setup_scatter_direct(slot, &fis, sg_list)
                .and_then(|()| self.issue_ncq_command(slot, 0, sg_list.len() as u16))
        } else {
            self.setup_scatter_command(slot, &fis, expected_size)
                .and_then(|()| self.issue_ncq_command(slot, 0, num_pages as u16))
        };

        if let Err(e) = setup {
            self.unwind_ncq_op(slot, &op);
            self.exit_ncq_mode();
            handle.complete(Err(ahci_err_to_block(e)));
            return Ok(handle);
        }

        // Open the slot for IRQ-side completion. Release pairs with the
        // dispatcher's Acquire load in `complete_ncq_slot`.
        op.issued.store(true, Ordering::Release);

        // Self-check: the drive may have completed between `issue_ncq_command`
        // and this store, and an IRQ may have fired during the
        // `!issued`-skip window. Run completion manually if SACT[slot] is
        // already clear. The CAS inside `complete_ncq_slot` makes this
        // race-safe versus a concurrent IRQ.
        let sact = unsafe { ptr::read_volatile(&raw const (*self.port_regs).sact) };
        if sact & (1u32 << slot) == 0 {
            self.complete_ncq_slot(slot, &op);
        }
        Ok(handle)
    }

    /// Async NCQ write. `fua` selects WRITE FPDMA QUEUED with FUA when
    /// supported by the device; the FUA-fallback (plain write + flush) lives
    /// in `write_with_fua_fallback` on the sync path and is only reachable
    /// from `submit_write`.
    pub fn submit_ncq_write(
        self: &Arc<Self>,
        lba: u64,
        sectors: u16,
        buffer: BlockBuffer,
        fua: bool,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        let expected_size = sectors as usize * 512;
        if sectors == 0 || buffer.len() < expected_size {
            return Err(BlockError::InvalidArg);
        }
        let num_pages = expected_size.div_ceil(4096);
        if num_pages > NCQ_PAGES_PER_SLOT {
            return Err(BlockError::InvalidArg);
        }

        self.enter_ncq_mode();

        let handle = BlockIoHandle::pending();
        let start_gen = self.reset_generation.load(Ordering::Acquire);

        let sg = virt_buffer_to_sg_list(buffer.as_ptr(), expected_size);
        let completion = if sg.is_some() {
            SlotCompletion::Direct
        } else {
            // Pool-path write: copy caller buffer into pool pages now, before
            // we issue the command.
            // SAFETY: caller's contract — buffer outlives the handle.
            SlotCompletion::PoolWrite
        };

        // For pool-path write we must copy the caller's data into the slot
        // pool BEFORE installing the op (the op stores the buffer, but the
        // pool is the actual DMA source). Doing it here keeps the
        // copy outside the op-install path.
        if sg.is_none() {
            // We don't yet know the slot. Allocate first, then copy.
        }

        let (slot, op) = self.install_ncq_op(handle.clone(), buffer, completion, start_gen);

        if sg.is_none() {
            // Pool path: copy caller buffer (held in op.buffer) into the
            // per-slot pool pages now.
            let pool = self.slot_pool(slot);
            let src = op.buffer.as_ptr();
            let mut offset = 0;
            for i in 0..num_pages {
                let copy_len = (expected_size - offset).min(4096);
                unsafe {
                    ptr::copy_nonoverlapping(src.add(offset), pool.pages[i].as_ptr(), copy_len);
                }
                offset += copy_len;
            }
        }

        let fis = if fua {
            FisRegH2D::new_write_fpdma_queued_fua(lba, sectors, slot as u8)
        } else {
            FisRegH2D::new_write_fpdma_queued(lba, sectors, slot as u8)
        };
        let setup = if let Some(ref sg_list) = sg {
            self.setup_scatter_direct(slot, &fis, sg_list)
                .and_then(|()| self.issue_ncq_command(slot, CMD_HEADER_WRITE, sg_list.len() as u16))
        } else {
            self.setup_scatter_command(slot, &fis, expected_size)
                .and_then(|()| self.issue_ncq_command(slot, CMD_HEADER_WRITE, num_pages as u16))
        };

        if let Err(e) = setup {
            self.unwind_ncq_op(slot, &op);
            self.exit_ncq_mode();
            handle.complete(Err(ahci_err_to_block(e)));
            return Ok(handle);
        }

        // See `submit_ncq_read` for the issued+self-check rationale.
        op.issued.store(true, Ordering::Release);
        let sact = unsafe { ptr::read_volatile(&raw const (*self.port_regs).sact) };
        if sact & (1u32 << slot) == 0 {
            self.complete_ncq_slot(slot, &op);
        }
        Ok(handle)
    }

    /// IRQ-side completion for a single slot. Called by `on_port_irq` when
    /// `SACT[slot]` has cleared. CAS Pending→Completed gates the cleanup; the
    /// reset-generation check guards against COMRESET clearing SACT.
    fn complete_ncq_slot(&self, slot: usize, op: &Arc<AhciNcqOp>) {
        // If the submitter hasn't issued the command yet, `SACT[slot] == 0`
        // is a false positive — the op was just installed in `ncq_waiters`
        // by `install_ncq_op` and the hardware write to SACT/CI is still
        // pending. Skip; the submitter's post-issue self-check (or a later
        // IRQ) will catch the real completion.
        if !op.issued.load(Ordering::Acquire) {
            return;
        }
        if op
            .state
            .compare_exchange(
                SLOT_PENDING,
                SLOT_COMPLETED,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return; // already terminal (cancel raced and won)
        }
        let cur_gen = self.reset_generation.load(Ordering::Acquire);
        let result = if cur_gen != op.start_gen {
            Err(BlockError::Io)
        } else {
            if let SlotCompletion::PoolRead {
                num_pages,
                expected_size,
            } = op.completion
            {
                let pool = self.slot_pool(slot);
                let dest = op.buffer.as_mut_ptr();
                let mut offset = 0;
                for i in 0..num_pages {
                    let copy_len = (expected_size - offset).min(4096);
                    unsafe {
                        ptr::copy_nonoverlapping(
                            pool.pages[i].as_ptr(),
                            dest.add(offset),
                            copy_len,
                        );
                    }
                    offset += copy_len;
                }
            }
            Ok(())
        };

        op.handle.complete(result);
        if let Some(t) = op.submitter.upgrade() {
            t.owned_ops_remove(Arc::as_ptr(op) as *const ());
        }
        **ranked_lock!(
            RANK_AHCI_SLOT,
            "AhciPort.ncq_waiters",
            self.ncq_waiters[slot]
        ) = None;
        self.free_slot(slot);
        self.exit_ncq_mode();
    }

    /// IRQ-side failure for every in-flight NCQ op. Used by the TFES path.
    fn fail_all_ncq_slots(&self, err: BlockError) {
        for slot in 0..AHCI_CMD_SLOTS {
            let op = {
                ranked_lock!(
                    RANK_AHCI_SLOT,
                    "AhciPort.ncq_waiters",
                    self.ncq_waiters[slot]
                )
                .clone()
            };
            let Some(op) = op else {
                continue;
            };
            // Submitter is still in setup; their post-issue path will see
            // the post-restart reset_generation mismatch and fail itself.
            if !op.issued.load(Ordering::Acquire) {
                continue;
            }
            if op
                .state
                .compare_exchange(
                    SLOT_PENDING,
                    SLOT_COMPLETED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                continue;
            }
            op.handle.complete(Err(err));
            if let Some(t) = op.submitter.upgrade() {
                t.owned_ops_remove(Arc::as_ptr(&op) as *const ());
            }
            **ranked_lock!(
                RANK_AHCI_SLOT,
                "AhciPort.ncq_waiters",
                self.ncq_waiters[slot]
            ) = None;
            self.free_slot(slot);
            self.exit_ncq_mode();
        }
    }

    /// Drop a stranded NCQ op from `ncq_waiters` when the cancel path wins
    /// the CAS. Called only from `AhciNcqOp::cancel`. The state machine has
    /// already transitioned Pending→Cancelled and the handle has been
    /// completed; here we just reclaim the hardware slot.
    pub fn release_orphaned_ncq_slot(&self, slot: usize) {
        **ranked_lock!(
            RANK_AHCI_SLOT,
            "AhciPort.ncq_waiters",
            self.ncq_waiters[slot]
        ) = None;
        self.free_slot(slot);
        self.exit_ncq_mode();
    }

    /// Per-port IRQ pass entry point. Replaces the old
    /// `wake_all_slot_waiters` walk. Detects TFES + restarts the port if
    /// needed; otherwise completes any NCQ slot whose `SACT` bit has cleared,
    /// and wakes legacy slot waiters so the sync poll loop can re-check `CI`.
    pub fn on_port_irq(self: &Arc<Self>, port_is: u32) {
        if port_is & PORT_IS_TFES != 0 {
            let tfd = unsafe { ptr::read_volatile(&raw const (*self.port_regs).tfd) };
            log!(
                "AHCI port {}: TFES status={:#x} error={:#x}; failing all in-flight NCQ slots",
                self.port_idx,
                tfd & 0xFF,
                (tfd >> 8) & 0xFF
            );
            self.fail_all_ncq_slots(BlockError::Io);
            // Only one thread restarts (re-entrancy guard); subsequent IRQs
            // before restart completes are no-ops at this branch.
            if self
                .restarting
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                let _ = self.restart_port();
                self.restarting.store(false, Ordering::Release);
            }
            return;
        }

        // NCQ slots: check SACT for clears
        let sact = unsafe { ptr::read_volatile(&raw const (*self.port_regs).sact) };
        for slot in 0..AHCI_CMD_SLOTS {
            let op = {
                ranked_lock!(
                    RANK_AHCI_SLOT,
                    "AhciPort.ncq_waiters",
                    self.ncq_waiters[slot]
                )
                .clone()
            };
            if let Some(op) = op {
                if sact & (1 << slot) == 0 {
                    self.complete_ncq_slot(slot, &op);
                }
            }
        }

        // Legacy slots: wake submitters polling CI.
        for slot in 0..AHCI_CMD_SLOTS {
            let op = {
                ranked_lock!(
                    RANK_AHCI_SLOT,
                    "AhciPort.slot_waiters",
                    self.slot_waiters[slot]
                )
                .clone()
            };
            if let Some(op) = op {
                if op.state.load(Ordering::Acquire) == SLOT_CANCELLED {
                    continue;
                }
                sched().wake_thread(&op.waiter, WakePriority::Interrupt);
                crate::drivers::ahci::AHCI_SLOT_WAKES.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Legacy (non-NCQ) ATA read / write -- serialized by legacy_lock
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Read sectors using legacy DMA EXT with scatter-gather. Serialized.
    fn legacy_ata_read(&self, lba: u64, buffer: &mut [u8], sectors: u16) -> Result<(), AhciError> {
        if sectors == 0 {
            return Ok(());
        }
        let expected_size = sectors as usize * 512;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }
        let num_pages = expected_size.div_ceil(4096);
        if num_pages > NCQ_PAGES_PER_SLOT {
            return Err(AhciError::IoError);
        }

        let _guard = ranked_lock!(RANK_AHCI_LEGACY, "AhciPort.legacy_lock", self.legacy_lock);

        self.with_slot_try(|slot, _op| {
            let fis = FisRegH2D::new_read_dma_ext(lba, sectors);
            let sg = virt_buffer_to_sg_list(buffer.as_ptr(), expected_size);

            if let Some(ref sg_list) = sg {
                self.setup_scatter_direct(slot, &fis, sg_list)?;
                let prdtl = sg_list.len() as u16;
                self.issue_command(slot, 0, prdtl)?;
            } else {
                self.setup_scatter_command(slot, &fis, expected_size)?;
                self.issue_command(slot, 0, num_pages as u16)?;
            }

            self.wait_for_completion(slot, Duration::from_secs(5))?;

            // Only copy from pool if we used the pool path
            if sg.is_none() {
                let pool = self.slot_pool(slot);
                let mut offset = 0;
                for i in 0..num_pages {
                    let copy_len = (expected_size - offset).min(4096);
                    unsafe {
                        ptr::copy_nonoverlapping(
                            pool.pages[i].as_ptr(),
                            buffer.as_mut_ptr().add(offset),
                            copy_len,
                        );
                    }
                    offset += copy_len;
                }
            }
            Ok(())
        })
    }

    /// Write sectors using legacy DMA EXT with scatter-gather. Serialized.
    /// If `fua` is true, uses WRITE DMA FUA EXT instead of WRITE DMA EXT.
    fn legacy_ata_write_inner(
        &self,
        lba: u64,
        buffer: &[u8],
        sectors: u16,
        fua: bool,
    ) -> Result<(), AhciError> {
        if sectors == 0 {
            return Ok(());
        }
        let expected_size = sectors as usize * 512;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }
        let num_pages = expected_size.div_ceil(4096);
        if num_pages > NCQ_PAGES_PER_SLOT {
            return Err(AhciError::IoError);
        }

        let _guard = ranked_lock!(RANK_AHCI_LEGACY, "AhciPort.legacy_lock", self.legacy_lock);

        self.with_slot_try(|slot, _op| {
            let fis = if fua {
                FisRegH2D::new_write_dma_fua_ext(lba, sectors)
            } else {
                FisRegH2D::new_write_dma_ext(lba, sectors)
            };
            let sg = virt_buffer_to_sg_list(buffer.as_ptr(), expected_size);

            if let Some(ref sg_list) = sg {
                self.setup_scatter_direct(slot, &fis, sg_list)?;
                let prdtl = sg_list.len() as u16;
                self.issue_command(slot, CMD_HEADER_WRITE, prdtl)?;
            } else {
                // Fallback: copy to pool pages
                let pool = self.slot_pool(slot);
                let mut offset = 0;
                for i in 0..num_pages {
                    let copy_len = (expected_size - offset).min(4096);
                    unsafe {
                        ptr::copy_nonoverlapping(
                            buffer.as_ptr().add(offset),
                            pool.pages[i].as_ptr(),
                            copy_len,
                        );
                    }
                    offset += copy_len;
                }
                self.setup_scatter_command(slot, &fis, expected_size)?;
                self.issue_command(slot, CMD_HEADER_WRITE, num_pages as u16)?;
            }

            self.wait_for_completion(slot, Duration::from_secs(5))
        })
    }

    fn legacy_ata_write(&self, lba: u64, buffer: &[u8], sectors: u16) -> Result<(), AhciError> {
        self.legacy_ata_write_inner(lba, buffer, sectors, false)
    }
}

// ---------------------------------------------------------------------------
// Non-NCQ command execution (IDENTIFY, FLUSH, etc.)
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Execute a non-NCQ command with a single PRDT entry. Used for IDENTIFY and FLUSH.
    /// Serialized by legacy_lock (caller must hold it for NCQ-enabled ports).
    fn execute_command(
        &self,
        fis: &FisRegH2D,
        buffer_addr: PhysAddr,
        buffer_size: usize,
        flags: u16,
        timeout: Duration,
    ) -> Result<(), AhciError> {
        self.with_slot_try(|slot, _op| {
            self.setup_command_table(slot, fis, buffer_addr, buffer_size)?;
            let prdtl = if buffer_size > 0 { 1 } else { 0 };
            self.issue_command(slot, flags, prdtl)?;
            self.wait_for_completion(slot, timeout)
        })
    }
}

// ---------------------------------------------------------------------------
// High-level dispatch (public API)
//
// `read_sectors` / `write_sectors` / `read_sectors_batch` are NOT exposed at
// this level anymore. The kernel-wide surface is the `AsyncBlockDevice` impl
// further down, which dispatches: NCQ-enabled ATA → `submit_ncq_*` (real
// async), legacy ATA / ATAPI → inline sync wrapper that pre-completes the
// handle.
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Flush write cache to disk. Drains NCQ before issuing FLUSH CACHE EXT.
    ///
    /// On NCQ-enabled ports `enter_legacy_mode` provides its own mutual
    /// exclusion via `mode==-1`, so taking `legacy_lock` is redundant AND
    /// dangerous: holding the lock across the park-on-NCQ-drain converts any
    /// NCQ stall into a freeze of every other legacy-path caller. Only the
    /// non-NCQ branch uses `legacy_lock`.
    pub fn flush_cache(&self) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata if self.ncq_enabled() => {
                self.enter_legacy_mode();
                let result = self.execute_command(
                    &FisRegH2D::new_flush_cache(),
                    PhysAddr::zero(),
                    0,
                    0,
                    Duration::from_secs(5),
                );
                self.exit_legacy_mode();
                result
            }
            DeviceType::Ata => {
                let _guard =
                    ranked_lock!(RANK_AHCI_LEGACY, "AhciPort.legacy_lock", self.legacy_lock);
                self.execute_command(
                    &FisRegH2D::new_flush_cache(),
                    PhysAddr::zero(),
                    0,
                    0,
                    Duration::from_secs(5),
                )
            }
            DeviceType::Atapi => Ok(()),
        }
    }

    /// Issue IDENTIFY DEVICE. Called during init after `set_weak_self`.
    pub fn identify_device(&self) -> Result<DeviceIdentifyInfo, AhciError> {
        match self.device_type {
            DeviceType::Ata => self.execute_ata_identify(),
            DeviceType::Atapi => self.execute_atapi_inquiry_as_identify(),
        }
    }

    fn execute_ata_identify(&self) -> Result<DeviceIdentifyInfo, AhciError> {
        let data_buffer = dma().allocate_sized(512)?;

        self.execute_command(
            &FisRegH2D::new_identify(),
            data_buffer.phys_addr(),
            512,
            0,
            Duration::from_secs(5),
        )?;

        let result = unsafe { &*data_buffer.as_ptr().cast::<[u8; 512]>() };
        let info = DeviceIdentifyInfo::from_identify_data(result);
        let _ = dma().dealloc(data_buffer);

        Ok(info)
    }
}

// ---------------------------------------------------------------------------
// ATAPI commands
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Execute an ATAPI (SCSI packet) command. Caller must hold legacy_lock.
    fn execute_atapi_command(
        &self,
        scsi_cmd: &[u8],
        buffer_addr: PhysAddr,
        buffer_size: usize,
        timeout: Duration,
    ) -> Result<(), AhciError> {
        self.with_slot_try(|slot, _op| {
            self.setup_atapi_command_table(slot, scsi_cmd, buffer_addr, buffer_size)?;
            let prdtl = if buffer_size > 0 { 1 } else { 0 };
            self.issue_command(slot, CMD_HEADER_ATAPI, prdtl)?;
            self.wait_for_completion(slot, timeout)
        })
    }

    /// SCSI INQUIRY, converted to DeviceIdentifyInfo.
    fn execute_atapi_inquiry_as_identify(&self) -> Result<DeviceIdentifyInfo, AhciError> {
        let inquiry_cmd = ScsiInquiry::new();
        let data_buffer = dma().allocate_sized(96)?;

        self.execute_atapi_command(
            bytemuck::bytes_of(&inquiry_cmd),
            data_buffer.phys_addr(),
            96,
            Duration::from_secs(5),
        )?;

        let inquiry_data = unsafe { core::slice::from_raw_parts(data_buffer.as_ptr(), 96) };
        let mut device_info = DeviceIdentifyInfo::from_scsi_inquiry(inquiry_data);

        if let Ok(capacity) = self.execute_atapi_read_capacity() {
            device_info.sectors = capacity.0;
            device_info.capacity_mb = (capacity.0 * capacity.1 as u64) / (1024 * 1024);
            device_info.capacity_gb = device_info.capacity_mb / 1024;
        }

        let _ = dma().dealloc(data_buffer);
        Ok(device_info)
    }

    fn execute_atapi_read_capacity(&self) -> Result<(u64, u32), AhciError> {
        let capacity_cmd = ScsiReadCapacity10::new();
        let data_buffer = dma().allocate_sized(8)?;

        self.execute_atapi_command(
            bytemuck::bytes_of(&capacity_cmd),
            data_buffer.phys_addr(),
            8,
            Duration::from_secs(5),
        )?;

        let data = unsafe { core::slice::from_raw_parts(data_buffer.as_ptr(), 8) };
        let last_lba = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let block_size = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

        let _ = dma().dealloc(data_buffer);

        Ok(((last_lba as u64) + 1, block_size))
    }

    /// ATAPI sector read via SCSI READ_10. Caller must hold legacy_lock.
    fn atapi_read(&self, lba: u64, buffer: &mut [u8], sectors: u16) -> Result<(), AhciError> {
        if sectors == 0 {
            return Ok(());
        }

        // ATAPI sectors are 2048 bytes; the API uses 512-byte sectors.
        let atapi_sectors = sectors.div_ceil(4);
        let atapi_lba = lba / 4;

        let atapi_buffer_size = atapi_sectors as usize * 2048;
        let mut temp_buffer = vec![0u8; atapi_buffer_size];

        self.execute_atapi_read_as_sectors(atapi_lba, &mut temp_buffer, atapi_sectors)?;

        let start_offset = (lba % 4) as usize * 512;
        let copy_size = (sectors as usize * 512).min(buffer.len());
        let available_data = temp_buffer.len().saturating_sub(start_offset);
        let actual_copy = copy_size.min(available_data);

        buffer[..actual_copy]
            .copy_from_slice(&temp_buffer[start_offset..start_offset + actual_copy]);
        Ok(())
    }

    fn execute_atapi_read_as_sectors(
        &self,
        lba: u64,
        buffer: &mut [u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        let expected_size = sectors as usize * 2048;
        if buffer.len() < expected_size {
            return Err(AhciError::IoError);
        }
        if lba > u32::MAX as u64 {
            return Err(AhciError::IoError);
        }

        let read_cmd = ScsiRead10::new(lba as u32, sectors);
        let data_buffer = dma().allocate_sized(expected_size)?;

        self.execute_atapi_command(
            bytemuck::bytes_of(&read_cmd),
            data_buffer.phys_addr(),
            expected_size,
            Duration::from_secs(10),
        )?;

        unsafe {
            ptr::copy_nonoverlapping(data_buffer.as_ptr(), buffer.as_mut_ptr(), expected_size);
        }

        let _ = dma().dealloc(data_buffer);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// AsyncBlockDevice implementation
// ---------------------------------------------------------------------------

use crate::drivers::block_io::{
    AsyncBlockDevice, BlockBuffer, BlockError, BlockIoHandle, WriteFlags,
};

fn ahci_err_to_block(e: AhciError) -> BlockError {
    match e {
        AhciError::CommandTimeout => BlockError::Timeout,
        AhciError::InvalidDevice | AhciError::PortNotReady => BlockError::DeviceGone,
        AhciError::DmaError(_) => BlockError::NoMemory,
        AhciError::InvalidSlot | AhciError::ReadOnly => BlockError::InvalidArg,
        AhciError::IoError => BlockError::Io,
    }
}

impl AhciPort {
    /// Write with FUA if supported, falling back to a plain write + flush
    /// so callers always get durability semantics even on QEMU where FUA is
    /// often unimplemented.
    /// Legacy/ATAPI write + flush sequence. Used only on non-NCQ ports or
    /// devices without FUA support — the NCQ FUA path goes through
    /// `submit_ncq_write(.., fua = true)` directly.
    fn legacy_write_then_flush(
        &self,
        lba: u64,
        data: &[u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata => self.legacy_ata_write(lba, data, sectors)?,
            DeviceType::Atapi => return Err(AhciError::ReadOnly),
        }
        self.flush_cache()
    }
}

/// Helper: pre-complete a handle from a sync `AhciError` result.
fn sync_handle(result: Result<(), AhciError>) -> Arc<BlockIoHandle> {
    let h = BlockIoHandle::pending();
    h.complete(result.map_err(ahci_err_to_block));
    h
}

impl AsyncBlockDevice for AhciPort {
    fn submit_read(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        if sectors == 0 || sectors > u16::MAX as u32 {
            return Err(BlockError::InvalidArg);
        }
        let sectors_u16 = sectors as u16;

        // NCQ-enabled ATA: real async submit.
        if self.device_type == DeviceType::Ata && self.ncq_enabled() {
            let arc_self = self
                .weak_self
                .get()
                .and_then(|w| w.upgrade())
                .expect("AhciPort::set_weak_self not called");
            return arc_self.submit_ncq_read(lba, sectors_u16, buffer);
        }

        // Legacy ATA / ATAPI: sync internally, pre-completed handle.
        let buf_len = buffer.len();
        let buf_ptr = buffer.as_mut_ptr();
        let result = match self.device_type {
            DeviceType::Ata => {
                let slice = unsafe { core::slice::from_raw_parts_mut(buf_ptr, buf_len) };
                self.legacy_ata_read(lba, slice, sectors_u16)
            }
            DeviceType::Atapi => {
                let _guard =
                    ranked_lock!(RANK_AHCI_LEGACY, "AhciPort.legacy_lock", self.legacy_lock);
                let slice = unsafe { core::slice::from_raw_parts_mut(buf_ptr, buf_len) };
                self.atapi_read(lba, slice, sectors_u16)
            }
        };
        drop(buffer);
        Ok(sync_handle(result))
    }

    fn submit_write(
        &self,
        lba: u64,
        sectors: u32,
        buffer: BlockBuffer,
        flags: WriteFlags,
    ) -> Result<Arc<BlockIoHandle>, BlockError> {
        if sectors == 0 || sectors > u16::MAX as u32 {
            return Err(BlockError::InvalidArg);
        }
        let sectors_u16 = sectors as u16;
        let needs_fua = flags.contains(WriteFlags::FUA);

        // NCQ-enabled ATA with hardware FUA support: real async submit with FUA bit.
        if self.device_type == DeviceType::Ata
            && self.ncq_enabled()
            && (!needs_fua || self.supports_fua())
        {
            let arc_self = self
                .weak_self
                .get()
                .and_then(|w| w.upgrade())
                .expect("AhciPort::set_weak_self not called");
            return arc_self.submit_ncq_write(lba, sectors_u16, buffer, needs_fua);
        }

        // Fallback: sync write + (optional flush) for durability.
        let buf_len = buffer.len();
        let buf_ptr = buffer.as_ptr();
        let result = match self.device_type {
            DeviceType::Ata => {
                let slice = unsafe { core::slice::from_raw_parts(buf_ptr, buf_len) };
                if needs_fua {
                    // FUA requested but device doesn't support it (or NCQ disabled):
                    // legacy write then flush_cache.
                    self.legacy_write_then_flush(lba, slice, sectors_u16)
                } else {
                    self.legacy_ata_write(lba, slice, sectors_u16)
                }
            }
            DeviceType::Atapi => Err(AhciError::ReadOnly),
        };
        drop(buffer);
        Ok(sync_handle(result))
    }

    fn submit_flush(&self) -> Result<Arc<BlockIoHandle>, BlockError> {
        Ok(sync_handle(self.flush_cache()))
    }

    fn submit_read_batch(
        &self,
        reqs: alloc::vec::Vec<(u64, u32, BlockBuffer)>,
    ) -> Result<alloc::vec::Vec<Arc<BlockIoHandle>>, BlockError> {
        // NCQ-enabled ATA: issue every command before waiting, returning
        // every handle to the caller. The submitter parks on each handle in
        // turn; the IRQ dispatcher completes them as the drive finishes.
        if self.device_type == DeviceType::Ata && self.ncq_enabled() {
            let mut handles = alloc::vec::Vec::with_capacity(reqs.len());
            for (lba, sectors, buf) in reqs {
                match self.submit_read(lba, sectors, buf) {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        let h = BlockIoHandle::pending();
                        h.complete(Err(e));
                        handles.push(h);
                    }
                }
            }
            return Ok(handles);
        }

        // Legacy / ATAPI: serial submit_read (each pre-completes its handle).
        reqs.into_iter()
            .map(|(lba, sectors, buf)| self.submit_read(lba, sectors, buf))
            .collect()
    }
}
