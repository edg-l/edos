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
    drivers::{
        ahci::{
            AhciError, DeviceType,
            cancel_op::{AhciSlotOp, SLOT_CANCELLED, SLOT_COMPLETED, SLOT_PENDING},
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

        // Try to merge with previous entry if physically contiguous
        if let Some(last) = sg.last_mut() {
            let (last_phys, last_len): &mut (PhysAddr, usize) = last;
            if *last_phys + *last_len as u64 == phys {
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
            weak_self: Once::new(),
            mode: AtomicI32::new(0),
            mode_waitq: WaitQueue::new(),
            slot_waitq: WaitQueue::new(),
            restarting: AtomicBool::new(false),
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
    fn restart_port(&self) -> Result<(), AhciError> {
        Self::stop_port(self.port_regs)?;
        unsafe {
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
        *self.slot_waiters[slot].lock() = None;
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
            let mut g = self.slot_waiters[slot].lock();
            let is_ours = g
                .as_ref()
                .map(|stored| Arc::as_ptr(stored) == op_ptr)
                .unwrap_or(false);
            if is_ours {
                *g = None;
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

    fn ncq_depth(&self) -> u8 {
        self.ncq_depth.load(Ordering::Acquire)
    }

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

    /// Allocate a slot (blocking), create an `AhciSlotOp`, register it with
    /// the submitter's `owned_ops`, run `f`, then clean up.
    ///
    /// Used by NCQ paths (`ncq_read`, `ncq_write_inner`, ATAPI execute_atapi_command)
    /// where we must wait for a slot rather than fail.
    ///
    /// `f` receives `(slot, &Arc<AhciSlotOp>)` and returns `Result<R, AhciError>`.
    /// After `f` returns:
    /// 1. On `Ok`: asserts the op reached `SLOT_COMPLETED`.
    /// 2. Removes the op from `owned_ops`.
    /// 3. Clears `slot_waiters[slot]` and returns the slot to the free pool.
    fn with_slot_blocking<R>(
        &self,
        f: impl FnOnce(usize, &Arc<AhciSlotOp>) -> Result<R, AhciError>,
    ) -> Result<R, AhciError> {
        let slot = self.allocate_slot_blocking();

        let port_weak = self
            .weak_self
            .get()
            .cloned()
            .expect("AhciPort::set_weak_self must be called before any command submission");
        let waiter = sched().current_thread_weak().unwrap_or_default();
        let op = Arc::new(AhciSlotOp::new(port_weak, slot, waiter));
        *self.slot_waiters[slot].lock() = Some(Arc::clone(&op));

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

        // Drive the state machine on success. NCQ paths reach SLOT_COMPLETED
        // via `wake_all_slot_waiters` (the IRQ-driven wake path CASes PENDING
        // → COMPLETED before waking). Non-NCQ / polling paths complete
        // synchronously inside `f` without touching state, so the CAS here
        // catches them up. The state-machine invariant after this line: an
        // Ok result means SLOT_COMPLETED; a PENDING-but-Ok op means cancel
        // raced and won (rare: thread death between f() return and this CAS).
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
        // `wake_all_slot_waiters` may have called `release_orphaned_slot` already
        // (Running→Dying race: wake_thread was a no-op, slot was released there).
        // The ptr_eq guard is load-bearing: without it, a concurrent
        // release_orphaned_slot (from wake_all_slot_waiters observing a Dying thread)
        // would clear slot_waiters and free the slot, and we'd double-free.
        let still_ours = {
            let mut g = self.slot_waiters[slot].lock();
            if g.as_ref().map(|o| Arc::ptr_eq(o, &op)).unwrap_or(false) {
                *g = None;
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
        *self.slot_waiters[slot].lock() = Some(Arc::clone(&op));

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
            let mut g = self.slot_waiters[slot].lock();
            if g.as_ref().map(|o| Arc::ptr_eq(o, &op)).unwrap_or(false) {
                *g = None;
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
                if byte_count == 0 {
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

        // Issue: write CI bit (read-modify-write under mmio_lock).
        let _lock = self.mmio_lock.lock();
        unsafe {
            let ci = ptr::read_volatile(&raw const (*self.port_regs).ci);
            ptr::write_volatile(&raw mut (*self.port_regs).ci, ci | (1 << slot));
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

        // Issue: SACT MUST be written before CI for NCQ commands.
        let _lock = self.mmio_lock.lock();
        unsafe {
            let sact = ptr::read_volatile(&raw const (*self.port_regs).sact);
            ptr::write_volatile(&raw mut (*self.port_regs).sact, sact | (1 << slot));
            let ci = ptr::read_volatile(&raw const (*self.port_regs).ci);
            ptr::write_volatile(&raw mut (*self.port_regs).ci, ci | (1 << slot));
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

    /// Wait for an NCQ command to complete (SACT bit clears).
    fn wait_for_ncq_completion(&self, slot: usize, timeout: Duration) -> Result<(), AhciError> {
        let start = crate::timer::Instant::now();
        let port_regs = self.port_regs;

        loop {
            let sact = unsafe { ptr::read_volatile(&raw const (*port_regs).sact) };
            if sact & (1 << slot) == 0 {
                return Ok(());
            }

            let is = unsafe { ptr::read_volatile(&raw const (*port_regs).is) };
            if is & PORT_IS_TFES != 0 {
                // Don't clear port IS here; the dispatch thread handles it.
                let tfd = unsafe { ptr::read_volatile(&raw const (*port_regs).tfd) };
                log!(
                    "AHCI port {}: NCQ error on slot {} - Status: {:#x}, Error: {:#x}",
                    self.port_idx,
                    slot,
                    tfd & 0xFF,
                    (tfd >> 8) & 0xFF
                );
                // NCQ error aborts ALL in-flight commands. Wake other waiters
                // so they observe the error and return IoError.
                self.wake_all_slot_waiters();
                // Only one thread runs restart_port; others just return the error.
                // Wait for all other NCQ threads to exit before restarting so no
                // thread issues a command to a stopped port.
                if self
                    .restarting
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                    .is_ok()
                {
                    // mode reaches 0 after all threads (including us) call exit_ncq_mode.
                    // We haven't exited yet, so wait for mode == 1 (just us left).
                    self.mode_waitq
                        .wait_until(|| self.mode.load(Ordering::Acquire) <= 1);
                    let _ = self.restart_port();
                    self.restarting.store(false, Ordering::Release);
                }
                return Err(AhciError::IoError);
            }

            if start.elapsed() >= timeout {
                log!("AHCI port {}: NCQ timeout on slot {}", self.port_idx, slot);
                return Err(AhciError::CommandTimeout);
            }

            sched().thread_park_while(|| {
                let sact = unsafe { ptr::read_volatile(&raw const (*port_regs).sact) };
                let is = unsafe { ptr::read_volatile(&raw const (*port_regs).is) };
                sact & (1 << slot) != 0 && is & PORT_IS_TFES == 0 && start.elapsed() < timeout
            });
        }
    }
}

// ---------------------------------------------------------------------------
// NCQ (FPDMA) read / write
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Read sectors using NCQ (READ FPDMA QUEUED). Concurrent-safe via &self.
    fn ncq_read(&self, lba: u64, buffer: &mut [u8], sectors: u16) -> Result<(), AhciError> {
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

        self.enter_ncq_mode();

        let result = self.with_slot_blocking(|slot, _op| {
            let fis = FisRegH2D::new_read_fpdma_queued(lba, sectors, slot as u8);
            let sg = virt_buffer_to_sg_list(buffer.as_ptr(), expected_size);

            if let Some(ref sg_list) = sg {
                self.setup_scatter_direct(slot, &fis, sg_list)?;
                let prdtl = sg_list.len() as u16;
                self.issue_ncq_command(slot, 0, prdtl)?;
            } else {
                self.setup_scatter_command(slot, &fis, expected_size)?;
                self.issue_ncq_command(slot, 0, num_pages as u16)?;
            }

            self.wait_for_ncq_completion(slot, Duration::from_secs(5))?;

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
        });

        self.exit_ncq_mode();

        result
    }

    /// Write sectors using NCQ (WRITE FPDMA QUEUED). Concurrent-safe via &self.
    /// If `fua` is true, uses WRITE FPDMA QUEUED with FUA bit set.
    fn ncq_write_inner(
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

        self.enter_ncq_mode();

        let result = self.with_slot_blocking(|slot, _op| {
            let fis = if fua {
                FisRegH2D::new_write_fpdma_queued_fua(lba, sectors, slot as u8)
            } else {
                FisRegH2D::new_write_fpdma_queued(lba, sectors, slot as u8)
            };
            let sg = virt_buffer_to_sg_list(buffer.as_ptr(), expected_size);

            if let Some(ref sg_list) = sg {
                self.setup_scatter_direct(slot, &fis, sg_list)?;
                let prdtl = sg_list.len() as u16;
                self.issue_ncq_command(slot, CMD_HEADER_WRITE, prdtl)?;
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
                self.issue_ncq_command(slot, CMD_HEADER_WRITE, num_pages as u16)?;
            }

            self.wait_for_ncq_completion(slot, Duration::from_secs(5))
        });

        self.exit_ncq_mode();

        result
    }

    fn ncq_write(&self, lba: u64, buffer: &[u8], sectors: u16) -> Result<(), AhciError> {
        self.ncq_write_inner(lba, buffer, sectors, false)
    }

    /// Read multiple disjoint sector ranges concurrently using NCQ.
    /// All commands are issued before any waits, maximizing drive parallelism.
    fn ncq_read_batch(&self, ranges: &mut [(u64, u16, &mut [u8])]) -> Result<(), AhciError> {
        if ranges.is_empty() {
            return Ok(());
        }

        // Cap concurrent commands to ncq_depth - 1 (leave a slot for flush/error).
        let max_batch = (self.ncq_depth() as usize).saturating_sub(1).max(1);

        self.enter_ncq_mode();

        let mut first_err: Option<AhciError> = None;

        // Per-thread handle for cancel registration.
        let current = sched().current_thread();
        let waiter_weak = sched().current_thread_weak().unwrap_or_default();
        let weak_port = self
            .weak_self
            .get()
            .cloned()
            .expect("AhciPort::set_weak_self not called before issuing I/O");

        for chunk in ranges.chunks_mut(max_batch) {
            // Allocate slots and issue all commands in this sub-batch.
            // Each slot gets its own AhciSlotOp for cancel tracking.
            let mut slots: heapless::Vec<usize, 32> = heapless::Vec::new();
            let mut ops: heapless::Vec<Option<Arc<AhciSlotOp>>, 32> = heapless::Vec::new();
            let mut direct: heapless::Vec<bool, 32> = heapless::Vec::new();

            for &(lba, sectors, ref buf) in chunk.iter() {
                if sectors == 0 {
                    let _ = slots.push(usize::MAX); // sentinel: no slot needed
                    let _ = ops.push(None);
                    let _ = direct.push(false);
                    continue;
                }
                let expected_size = sectors as usize * 512;
                let num_pages = expected_size.div_ceil(4096);
                if num_pages > NCQ_PAGES_PER_SLOT || buf.len() < expected_size {
                    first_err.get_or_insert(AhciError::IoError);
                    let _ = slots.push(usize::MAX);
                    let _ = ops.push(None);
                    let _ = direct.push(false);
                    continue;
                }

                let slot = self.allocate_slot_blocking();
                let op = Arc::new(AhciSlotOp::new(
                    weak_port.clone(),
                    slot,
                    waiter_weak.clone(),
                ));
                *self.slot_waiters[slot].lock() = Some(Arc::clone(&op));

                // Best-effort cancel registration. With OWNED_OPS_CAP=32 and
                // max_batch up to 31, push may rarely fail on the last slots.
                // A failed push means that slot won't be auto-cancelled on death;
                // all other batch slots will be. Log a warning.
                if let Some(ref t) = current {
                    if t.owned_ops_push(Arc::clone(&op) as ArcCancellableOp)
                        .is_err()
                    {
                        log!(
                            "AHCI port {}: owned_ops full for batch slot {}; cancel hookup skipped",
                            self.port_idx,
                            slot
                        );
                    }
                }

                let sg = virt_buffer_to_sg_list(buf.as_ptr(), expected_size);

                let setup_result = if let Some(ref sg_list) = sg {
                    self.setup_scatter_direct(
                        slot,
                        &FisRegH2D::new_read_fpdma_queued(lba, sectors, slot as u8),
                        sg_list,
                    )
                    .and_then(|()| self.issue_ncq_command(slot, 0, sg_list.len() as u16))
                } else {
                    self.setup_scatter_command(
                        slot,
                        &FisRegH2D::new_read_fpdma_queued(lba, sectors, slot as u8),
                        expected_size,
                    )
                    .and_then(|()| self.issue_ncq_command(slot, 0, num_pages as u16))
                };

                if let Err(e) = setup_result {
                    // Setup failed: remove from owned_ops, clear slot.
                    // CAS Pending→Cancelled so a concurrent IRQ's CAS to
                    // Completed fails and the IRQ path sees no work to do.
                    let _ = op.state.compare_exchange(
                        SLOT_PENDING,
                        SLOT_CANCELLED,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    if let Some(ref t) = current {
                        t.owned_ops_remove(Arc::as_ptr(&op) as *const ());
                    }
                    // Idempotent: only free if the slot is still ours.
                    let still_ours = {
                        let mut g = self.slot_waiters[slot].lock();
                        if g.as_ref().map(|o| Arc::ptr_eq(o, &op)).unwrap_or(false) {
                            *g = None;
                            true
                        } else {
                            false
                        }
                    };
                    if still_ours {
                        self.free_slot(slot);
                    }
                    first_err.get_or_insert(e);
                    let _ = slots.push(usize::MAX);
                    let _ = ops.push(None);
                    let _ = direct.push(false);
                    continue;
                }
                let _ = slots.push(slot);
                let _ = ops.push(Some(op));
                let _ = direct.push(sg.is_some());
            }

            // Wait for all issued commands and copy results.
            for (i, (_, sectors, buf)) in chunk.iter_mut().enumerate() {
                let slot = slots[i];
                if slot == usize::MAX {
                    continue; // skipped or errored during issue
                }

                let wait_result = self.wait_for_ncq_completion(slot, Duration::from_secs(5));

                if wait_result.is_ok() && !direct[i] {
                    // Pool path: copy from pool to buffer
                    let expected_size = *sectors as usize * 512;
                    let num_pages = expected_size.div_ceil(4096);
                    let pool = self.slot_pool(slot);
                    let mut offset = 0;
                    for p in 0..num_pages {
                        let copy_len = (expected_size - offset).min(4096);
                        unsafe {
                            ptr::copy_nonoverlapping(
                                pool.pages[p].as_ptr(),
                                buf.as_mut_ptr().add(offset),
                                copy_len,
                            );
                        }
                        offset += copy_len;
                    }
                } else if let Err(e) = wait_result {
                    first_err.get_or_insert(e);
                }

                // Deregister from owned_ops and release slot.
                // Use ptr_eq guard: wake_all_slot_waiters may have called
                // release_orphaned_slot already (Running→Dying race), so we
                // only free the slot if it is still ours. Without this check
                // we'd double-free the slot.
                if let Some(ref op) = ops[i] {
                    if let Some(ref t) = current {
                        t.owned_ops_remove(Arc::as_ptr(op) as *const ());
                    }
                    let still_ours = {
                        let mut g = self.slot_waiters[slot].lock();
                        if g.as_ref().map(|o| Arc::ptr_eq(o, op)).unwrap_or(false) {
                            *g = None;
                            true
                        } else {
                            false
                        }
                    };
                    if still_ours {
                        self.free_slot(slot);
                    }
                }
            }
        }

        self.exit_ncq_mode();

        match first_err {
            Some(e) => Err(e),
            None => Ok(()),
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

        let _guard = self.legacy_lock.lock();

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

        let _guard = self.legacy_lock.lock();

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
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Read sectors from the device. Dispatches to NCQ, legacy ATA, or ATAPI.
    pub fn read_sectors(&self, lba: u64, buffer: &mut [u8], sectors: u16) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata if self.ncq_enabled() => self.ncq_read(lba, buffer, sectors),
            DeviceType::Ata => self.legacy_ata_read(lba, buffer, sectors),
            DeviceType::Atapi => {
                let _guard = self.legacy_lock.lock();
                self.atapi_read(lba, buffer, sectors)
            }
        }
    }

    /// Read multiple disjoint sector ranges. NCQ ports issue all concurrently;
    /// non-NCQ and ATAPI fall back to sequential reads.
    pub fn read_sectors_batch(
        &self,
        ranges: &mut [(u64, u16, &mut [u8])],
    ) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata if self.ncq_enabled() => self.ncq_read_batch(ranges),
            _ => {
                for (lba, sectors, buf) in ranges.iter_mut() {
                    self.read_sectors(*lba, buf, *sectors)?;
                }
                Ok(())
            }
        }
    }

    /// Write sectors to the device. Dispatches to NCQ, legacy ATA, or ATAPI.
    pub fn write_sectors(&self, lba: u64, buffer: &[u8], sectors: u16) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata if self.ncq_enabled() => self.ncq_write(lba, buffer, sectors),
            DeviceType::Ata => self.legacy_ata_write(lba, buffer, sectors),
            DeviceType::Atapi => Err(AhciError::ReadOnly),
        }
    }

    /// Write sectors with Force Unit Access (bypasses drive write cache).
    /// Returns `IoError` if the device does not support FUA.
    pub fn write_sectors_fua(
        &self,
        lba: u64,
        buffer: &[u8],
        sectors: u16,
    ) -> Result<(), AhciError> {
        if !self.supports_fua() {
            return Err(AhciError::IoError);
        }
        match self.device_type {
            DeviceType::Ata if self.ncq_enabled() => {
                self.ncq_write_inner(lba, buffer, sectors, true)
            }
            DeviceType::Ata => self.legacy_ata_write_inner(lba, buffer, sectors, true),
            DeviceType::Atapi => Err(AhciError::ReadOnly),
        }
    }

    /// Flush write cache to disk. Drains NCQ before issuing FLUSH CACHE EXT.
    pub fn flush_cache(&self) -> Result<(), AhciError> {
        match self.device_type {
            DeviceType::Ata => {
                let _guard = self.legacy_lock.lock();
                if self.ncq_enabled() {
                    self.enter_legacy_mode();
                }
                let result = self.execute_command(
                    &FisRegH2D::new_flush_cache(),
                    PhysAddr::zero(),
                    0,
                    0,
                    Duration::from_secs(5),
                );
                if self.ncq_enabled() {
                    self.exit_legacy_mode();
                }
                result
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
// Interrupt helper
// ---------------------------------------------------------------------------

impl AhciPort {
    /// Wake all threads waiting on any slot. Called by the AHCI driver kthread
    /// after an interrupt signals command completion.
    ///
    /// Runs in thread context (NOT IRQ context); the per-slot `spin::Mutex` is
    /// safe here — see the `slot_waiters` field comment for IRQ-safety reasoning.
    ///
    /// # Cancel-race handling (Foundation #2)
    ///
    /// Each slot's `AhciSlotOp` has a `state` CAS machine:
    /// - **Pending → Completed** (we win): try to wake the submitter. If the
    ///   submitter's `Weak<Thread>` is gone or the thread is `Dying`, call
    ///   `release_orphaned_slot` ourselves so the slot is not leaked.
    /// - **Pending → Cancelled** (cancel path won): the canceller already called
    ///   `release_orphaned_slot`. Nothing to do.
    /// - **Already Completed** (double-interrupt, impossible in practice): panic
    ///   in debug mode, no-op in release.
    pub fn wake_all_slot_waiters(&self) {
        // AHCI's IRQ means "SOME slots completed; check SACT to find which."
        // It does NOT tell us which specific slot just completed. The correct
        // protocol is to wake every parked waiter and let them re-read SACT
        // in their `thread_park_while` closure (which will re-park if their
        // slot is still in-flight). Spurious wakes are expected and cheap.
        //
        // We do NOT touch `op.state` here. The submitter is authoritative on
        // completion (it sees SACT=0 for its slot and CASes PENDING→COMPLETED
        // in `with_slot_*` on the success path). The only state transitions
        // originating elsewhere are PENDING→CANCELLED (from `cancel()`) or
        // PENDING→COMPLETED (from the submitter). A CAS here would spuriously
        // "complete" slots that have not actually completed, hiding their
        // later real completion behind a CAS failure — hanging the waiter.
        for waiter_mutex in self.slot_waiters.iter() {
            let op_opt = waiter_mutex.lock().clone();
            let Some(op) = op_opt else {
                continue;
            };
            // If the op was already cancelled, the canceller already freed
            // the slot and cleared `slot_waiters` — our clone is just
            // keeping the Arc alive until this scope ends. Waking a
            // cancelled op's thread is harmless (wake_pending + dangling
            // Weak = silent no-op), but skip for clarity.
            if op.state.load(Ordering::Acquire) == SLOT_CANCELLED {
                continue;
            }
            sched().wake_thread(&op.waiter, WakePriority::Interrupt);
        }
    }
}
