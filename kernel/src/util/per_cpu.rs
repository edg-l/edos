use core::cell::{Cell, UnsafeCell};

use alloc::{boxed::Box, sync::Arc};
use x2apic::lapic::LocalApic;
use x86_64::{
    VirtAddr,
    registers::model_specific::{GsBase, KernelGsBase},
    structures::tss::TaskStateSegment,
};

use crate::{
    acpi::raw_current_apic_id,
    thread::{scheduler::Scheduler, thread::Thread},
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
    pub uaccess: UAccessState,
    /// Top of the per-CPU scheduler stack. The voluntary context-switch
    /// trampoline pivots RSP here before calling the transition fn and
    /// pick_and_run, so the outgoing thread's kernel stack is free as
    /// soon as the thread's state is published.
    pub scheduler_stack_top: Cell<u64>,
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
            uaccess: UAccessState::new(),
            scheduler_stack_top: Cell::new(0),
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

    /// Set the current thread.
    pub unsafe fn set_current_thread(&self, thread: Option<Arc<Thread>>) {
        unsafe { *self.current_thread.get() = thread }
    }
}

/// Returns a shared reference to the current CPU's PerCpuData via GS base.
#[inline(always)]
pub fn get_percpu_data() -> &'static PerCpuData {
    // Always keep GSBase == KernelGSBase to avoid ambiguity across SWAPGS.
    let base = GsBase::read().as_u64() as *mut PerCpuData;
    unsafe { &*base }
}

/// Allocate and install per-CPU data for the current CPU and set GS bases.
/// Must be called once per CPU very early during bring-up (before GDT/IDT use it).
pub unsafe fn init_gs_for_this_cpu(lapic_id: u32) -> &'static PerCpuData {
    let percpu_ptr: *mut PerCpuData = Box::leak(Box::new(PerCpuData::new()));
    unsafe { (*percpu_ptr).lapic_id.set(lapic_id) };
    let addr = VirtAddr::new(percpu_ptr as u64);
    // Set both to the same value so `swapgs` does not change effective base
    // and `get_percpu_data()` works uniformly.
    GsBase::write(addr);
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
    GsBase::write(addr);
    KernelGsBase::write(addr);
    unsafe { &*ptr }
}
