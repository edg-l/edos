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

    /// Mutable ref to TSS. Only call from owning CPU.
    pub unsafe fn tss_mut(&self) -> &mut TaskStateSegment {
        unsafe { &mut *self.tss.get() }
    }

    /// Clone the current thread Arc.
    pub fn current_thread(&self) -> Option<Arc<Thread>> {
        unsafe { (*self.current_thread.get()).clone() }
    }

    /// Borrow the current thread without cloning the Arc. Cheap fast path
    /// for hot kernel sites (e.g. syscall dispatch instrumentation) that
    /// want a single field access without paying for an atomic refcount
    /// bump and drop. Caller must not retain the borrow across a context
    /// switch; the safest pattern is a closure scope.
    #[inline]
    pub fn with_current_thread<R>(&self, f: impl FnOnce(&Thread) -> R) -> Option<R> {
        unsafe { (*self.current_thread.get()).as_deref().map(f) }
    }

    /// Set the current thread, dropping any cached `UserThreadInfo` with it.
    ///
    /// The drop is what keeps a thread's fd table, working directory and
    /// address space from outliving it by however long this CPU takes to run
    /// another user thread.
    pub unsafe fn set_current_thread(&self, thread: Option<Arc<Thread>>) {
        unsafe {
            *self.current_info.get() = None;
            *self.current_thread.get() = thread;
        }
    }

    /// This CPU's cached `UserThreadInfo` for `tid`, or `None` when it holds
    /// another thread's or nothing.
    pub fn cached_thread_info(&self, tid: ThreadId) -> Option<Arc<IrqSpinlock<UserThreadInfo>>> {
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
        unsafe { crate::drivers::fpu::enable_fsgsbase() };
    }
}

/// Enable FSGSBASE on this AP (CR4 is per-CPU). Only call if BSP detected support.
pub fn enable_fsgsbase_on_ap() {
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
        unsafe { crate::drivers::fpu::enable_fsgsbase() };
    }
}

#[inline(always)]
fn write_gs_base(addr: VirtAddr) {
    if HAS_FSGSBASE.load(Ordering::Relaxed) {
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
        unsafe {
            core::arch::asm!("rdgsbase {}", out(reg) base, options(nomem, nostack, preserves_flags));
        }
    } else {
        base = GsBase::read().as_u64();
    }
    unsafe { &*(base as *const PerCpuData) }
}

/// Allocate and install per-CPU data for the current CPU and set GS bases.
/// Must be called once per CPU very early during bring-up (before GDT/IDT use it).
pub unsafe fn init_gs_for_this_cpu(lapic_id: u32) -> &'static PerCpuData {
    let percpu_ptr: *mut PerCpuData = Box::leak(Box::new(PerCpuData::new()));
    unsafe { (*percpu_ptr).lapic_id.set(lapic_id) };
    let addr = VirtAddr::new(percpu_ptr as u64);
    // Set both to the same value so `swapgs` does not change effective base
    // and `get_percpu_data()` works uniformly.
    write_gs_base(addr);
    KernelGsBase::write(addr);
    unsafe { &*percpu_ptr }
}

/// BSP-only: install a statically allocated PerCpuData and set GS bases.
/// Call this in early BSP boot before the heap is initialized.
pub unsafe fn init_gs_for_bsp_static() -> &'static PerCpuData {
    static BSP_PCPU: PerCpuData = PerCpuData::new();
    let ptr: *const PerCpuData = &BSP_PCPU;
    unsafe {
        (*ptr).lapic_id.set(raw_current_apic_id());
    }
    let addr = VirtAddr::new(ptr as u64);
    write_gs_base(addr);
    KernelGsBase::write(addr);
    unsafe { &*ptr }
}
