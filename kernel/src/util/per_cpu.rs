use core::cell::{Cell, UnsafeCell};

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, Ordering};
use x2apic::lapic::LocalApic;

use x86_64::{
    VirtAddr,
    registers::model_specific::{FsBase, GsBase, KernelGsBase},
    structures::tss::TaskStateSegment,
};

use crate::thread::preempt::PreemptCount;
use crate::{
    acpi::raw_current_apic_id,
    allocator::PerCpuCacheCell,
    thread::{
        UserThreadInfo,
        irqlock::IrqSpinlock,
        scheduler::Scheduler,
        thread::{Thread, ThreadId},
    },
    util::uaccess::UAccessState,
};

/// Per CPU data
/// Note: try to keep it small.
#[repr(C, align(64))]
pub struct PerCpuData {
    pub user_rsp: Cell<u64>,   // Offset 0 - save user stack
    pub kernel_rsp: Cell<u64>, // Offset 8 - kernel stack for syscalls
    pub lapic_id: Cell<u32>,
    tss: UnsafeCell<TaskStateSegment>,
    // Cache line
    pub lapic: Cell<*mut LocalApic>,
    pub scheduler: Cell<*mut Scheduler>,
    current_thread: UnsafeCell<Option<Arc<Thread>>>,
    /// The running thread's `UserThreadInfo`, so a syscall does not pay a
    /// registry lookup to reach it.
    ///
    /// Keyed by thread id rather than trusted blindly. Ids are never reused, so
    /// a matching key is a match for the thread actually running here, and the
    /// cache stays correct even if some path publishes a thread without going
    /// through `set_current_thread`.
    current_info: UnsafeCell<Option<(ThreadId, Arc<IrqSpinlock<UserThreadInfo>>)>>,
    /// Thread id whose FPU state is loaded in this CPU's registers, or 0 for
    /// none. See `Thread::fpu_cpu` for why one side is not enough.
    pub fpu_owner: Cell<u64>,
    pub uaccess: UAccessState,
    /// Top of the per-CPU scheduler stack. The voluntary context-switch
    /// trampoline pivots RSP here before calling the transition fn and
    /// pick_and_run, so the outgoing thread's kernel stack is free as
    /// soon as the thread's state is published.
    pub scheduler_stack_top: Cell<u64>,
    /// Per-CPU heap allocation cache (avoids global heap lock contention).
    pub heap_cache: PerCpuCacheCell,
    /// Nesting count of preemption suppression. Non-zero means a spin lock is
    /// held here and `maybe_preempt` must leave this CPU alone.
    pub preempt_count: PreemptCount,
}

// SAFETY: PerCpuData is only accessed by its owning CPU via GS base.
// No cross-CPU access occurs. The Cell/UnsafeCell fields are !Sync but
// the per-CPU isolation makes sharing safe.
unsafe impl Sync for PerCpuData {}
// SAFETY: the same argument as the impl above.
unsafe impl Send for PerCpuData {}

impl PerCpuData {
    pub const fn new() -> Self {
        Self {
            user_rsp: Cell::new(0),
            kernel_rsp: Cell::new(0),
            lapic_id: Cell::new(0),
            tss: UnsafeCell::new(TaskStateSegment::new()),
            lapic: Cell::new(core::ptr::null_mut()),
            scheduler: Cell::new(core::ptr::null_mut()),
            current_thread: UnsafeCell::new(None),
            current_info: UnsafeCell::new(None),
            fpu_owner: Cell::new(0),
            uaccess: UAccessState::new(),
            scheduler_stack_top: Cell::new(0),
            heap_cache: PerCpuCacheCell::new(),
            preempt_count: PreemptCount::new(0),
        }
    }

    /// Raw pointer to the TSS for GDT descriptor setup.
    pub fn tss_ptr(&self) -> *const TaskStateSegment {
        self.tss.get() as *const _
    }

    /// The TSS this CPU loads its ring-0 and IST stack pointers from.
    ///
    /// # Safety
    /// The caller must be running on the CPU this `PerCpuData` belongs to, must
    /// not already hold a reference into the same TSS, and must not let the
    /// returned reference outlive a context switch.
    // Per-CPU interior-mutable state: the owning CPU is the only accessor, and
    // `&self` is the only handle it has, so the aliasing the lint warns about
    // cannot arise.
    #[expect(
        clippy::mut_from_ref,
        reason = "the TSS belongs to the calling CPU; the contract is on the unsafe fn"
    )]
    pub unsafe fn tss_mut(&self) -> &mut TaskStateSegment {
        // SAFETY: the caller upholds that it runs on the owning CPU and holds
        // no other reference into this TSS, so this is the only live borrow.
        unsafe { &mut *self.tss.get() }
    }

    /// Clone the current thread Arc.
    pub fn current_thread(&self) -> Option<Arc<Thread>> {
        // SAFETY: this slot is only ever written by `set_current_thread` on the
        // owning CPU, and a `&self` here was reached through that CPU's GS
        // base, so no other CPU can be writing it while this clone reads it.
        unsafe { (*self.current_thread.get()).clone() }
    }

    /// Borrow the current thread without cloning the Arc. Cheap fast path
    /// for hot kernel sites (e.g. syscall dispatch instrumentation) that
    /// want a single field access without paying for an atomic refcount
    /// bump and drop. Caller must not retain the borrow across a context
    /// switch; the safest pattern is a closure scope.
    #[inline]
    pub fn with_current_thread<R>(&self, f: impl FnOnce(&Thread) -> R) -> Option<R> {
        // SAFETY: as in `current_thread`, the owning CPU is the only writer.
        // The borrow does not escape `f`, and a switch on this CPU can only
        // happen after `f` returns.
        unsafe { (*self.current_thread.get()).as_deref().map(f) }
    }

    /// Set the current thread, dropping any cached `UserThreadInfo` with it.
    ///
    /// The drop is what keeps a thread's fd table, working directory and
    /// address space from outliving it by however long this CPU takes to run
    /// another user thread.
    ///
    /// # Safety
    /// The caller must be on the CPU this `PerCpuData` belongs to with
    /// preemption or interrupts held off, so the store cannot land on the CPU
    /// the reader has since migrated away from, and must hold no outstanding
    /// borrow handed out by `with_current_thread`.
    pub unsafe fn set_current_thread(&self, thread: Option<Arc<Thread>>) {
        // SAFETY: the caller guarantees it cannot migrate, so this CPU is the
        // only accessor. The info cache is cleared first: it keys off the
        // outgoing thread and must never be seen against the incoming one.
        unsafe {
            *self.current_info.get() = None;
            *self.current_thread.get() = thread;
        }
    }

    /// This CPU's cached `UserThreadInfo` for `tid`, or `None` when it holds
    /// another thread's or nothing.
    pub fn cached_thread_info(&self, tid: ThreadId) -> Option<Arc<IrqSpinlock<UserThreadInfo>>> {
        // SAFETY: written only by `cache_thread_info` and `set_current_thread`,
        // both of which run on the owning CPU with migration held off, so no
        // concurrent writer exists while this reads.
        unsafe {
            match &*self.current_info.get() {
                Some((cached, info)) if *cached == tid => Some(info.clone()),
                _ => None,
            }
        }
    }

    /// Remember `info` as `tid`'s, for the rest of this thread's turn here.
    ///
    /// # Safety
    /// Interrupts must be off: this CPU's slot is being written, and a
    /// migration between the read of the current thread and this store would
    /// cache the entry against the wrong CPU.
    pub unsafe fn cache_thread_info(&self, tid: ThreadId, info: Arc<IrqSpinlock<UserThreadInfo>>) {
        // SAFETY: the caller guarantees interrupts are off, so this CPU is the
        // only accessor of the slot for the duration of the store.
        unsafe { *self.current_info.get() = Some((tid, info)) }
    }
}

/// Write GS base using `wrgsbase` (~1 cycle vs ~30 for wrmsr).
/// Set by BSP after probing CPUID for FSGSBASE support.
static HAS_FSGSBASE: AtomicBool = AtomicBool::new(false);

/// Probe CPUID and enable FSGSBASE if supported. Called once on BSP;
/// the result applies to all CPUs (homogeneous feature set).
pub fn probe_and_enable_fsgsbase() {
    // CPUID leaf 7, sub-leaf 0: EBX bit 0 = FSGSBASE
    // rbx is reserved by LLVM, so save/restore it manually.
    let ebx: u32;
    // SAFETY: `cpuid` is architecturally available on every CPU this kernel
    // runs on and has no side effects beyond the four output registers, all of
    // which are declared. rbx is LLVM-reserved, so the block saves and restores
    // it around the instruction rather than naming it as an operand.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "mov eax, 7",
            "xor ecx, ecx",
            "cpuid",
            "mov {0:e}, ebx",
            "pop rbx",
            out(reg) ebx,
            out("eax") _,
            out("ecx") _,
            out("edx") _,
        );
    }
    if ebx & 1 != 0 {
        HAS_FSGSBASE.store(true, Ordering::Relaxed);
        // SAFETY: CPUID.(EAX=7,ECX=0):EBX[0] was just read as set, which is
        // exactly the condition setting CR4.FSGSBASE requires.
        unsafe { crate::drivers::fpu::enable_fsgsbase() };
    }
}

/// Enable FSGSBASE on this AP (CR4 is per-CPU). Only call if BSP detected support.
pub fn enable_fsgsbase_on_ap() {
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
        // SAFETY: the flag is only set by `probe_and_enable_fsgsbase` after
        // CPUID reported the feature, and the CPUs are a homogeneous set, so
        // this AP has it too. CR4 is per-CPU, hence the second write here.
        unsafe { crate::drivers::fpu::enable_fsgsbase() };
    }
}

#[inline(always)]
fn write_gs_base(addr: VirtAddr) {
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
        // SAFETY: the flag means CR4.FSGSBASE is set on this CPU, so the
        // instruction is legal. `addr` is a canonical kernel address, which is
        // what `wrgsbase` requires; a non-canonical value would #GP.
        unsafe {
            core::arch::asm!("wrgsbase {}", in(reg) addr.as_u64(), options(nomem, nostack, preserves_flags));
        }
    } else {
        GsBase::write(addr);
    }
}

/// Read the FS base, which is where a user thread's TLS block lives.
///
/// `rdfsbase` when the CPU has it, `rdmsr` otherwise. The context switch reads
/// this on the way out and writes it on the way in, and the two MSR accesses
/// together measured 104 ns of a 1270 ns switch against a cycle or two for the
/// instructions.
#[inline(always)]
pub fn read_fs_base() -> VirtAddr {
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
        let base: u64;
        // SAFETY: the flag means CR4.FSGSBASE is set on this CPU. `rdfsbase`
        // only writes the named output register.
        unsafe {
            core::arch::asm!("rdfsbase {}", out(reg) base, options(nomem, nostack, preserves_flags));
        }
        VirtAddr::new(base)
    } else {
        FsBase::read()
    }
}

/// Write the FS base. See [`read_fs_base`].
#[inline(always)]
pub fn write_fs_base(addr: VirtAddr) {
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
        // SAFETY: the flag means CR4.FSGSBASE is set on this CPU. `addr` comes
        // from `arch_prctl(ARCH_SET_FS)` or a saved thread context, both of
        // which are canonical-checked before reaching here.
        unsafe {
            core::arch::asm!("wrfsbase {}", in(reg) addr.as_u64(), options(nomem, nostack, preserves_flags));
        }
    } else {
        FsBase::write(addr);
    }
}

/// Returns a shared reference to the current CPU's PerCpuData via GS base.
/// Uses `rdgsbase` (~1 cycle) when available, falls back to `rdmsr` (~30 cycles).
#[inline(always)]
pub fn get_percpu_data() -> &'static PerCpuData {
    let base: u64;
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
        // SAFETY: the flag means CR4.FSGSBASE is set on this CPU. `rdgsbase`
        // only writes the named output register.
        unsafe {
            core::arch::asm!("rdgsbase {}", out(reg) base, options(nomem, nostack, preserves_flags));
        }
    } else {
        base = GsBase::read().as_u64();
    }
    // SAFETY: `init_gs_for_this_cpu` or `init_gs_for_bsp_static` ran on this
    // CPU before anything calls this, and both point GS at a leaked `Box` or a
    // `static`, so the pointee lives for the rest of the boot. The reference is
    // shared, which is why every mutable field is a `Cell` or `UnsafeCell`.
    unsafe { &*(base as *const PerCpuData) }
}

/// Allocate and install per-CPU data for the current CPU and set GS bases.
///
/// # Safety
/// Call once per CPU, on the CPU itself, before anything reads GS: the GDT and
/// IDT setup that follows dereferences what this installs. Calling it twice on
/// one CPU leaks the first block and orphans every reference into it.
pub unsafe fn init_gs_for_this_cpu(lapic_id: u32) -> &'static PerCpuData {
    let percpu_ptr: *mut PerCpuData = Box::leak(Box::new(PerCpuData::new()));
    // SAFETY: the pointer comes from `Box::leak`, so it is valid, aligned and
    // has no other reference to it yet.
    unsafe { (*percpu_ptr).lapic_id.set(lapic_id) };
    let addr = VirtAddr::new(percpu_ptr as u64);
    // Set both to the same value so `swapgs` does not change effective base
    // and `get_percpu_data()` works uniformly.
    write_gs_base(addr);
    KernelGsBase::write(addr);
    // SAFETY: the allocation is leaked, so `'static` is the truth about it.
    unsafe { &*percpu_ptr }
}

/// BSP-only: install a statically allocated PerCpuData and set GS bases.
///
/// # Safety
/// Call once, on the BSP, before the heap exists -- which is the whole reason
/// this exists beside [`init_gs_for_this_cpu`]. An AP calling it would point
/// its GS at the BSP's block and silently share every per-CPU field.
pub unsafe fn init_gs_for_bsp_static() -> &'static PerCpuData {
    static BSP_PCPU: PerCpuData = PerCpuData::new();
    let ptr: *const PerCpuData = &BSP_PCPU;
    // SAFETY: `ptr` names a `static`, so it is valid and aligned for the whole
    // boot. `lapic_id` is a `Cell`, so this shared-reference store is sound.
    unsafe {
        (*ptr).lapic_id.set(raw_current_apic_id());
    }
    let addr = VirtAddr::new(ptr as u64);
    write_gs_base(addr);
    KernelGsBase::write(addr);
    // SAFETY: `ptr` names a `static`, which outlives every caller.
    unsafe { &*ptr }
}
