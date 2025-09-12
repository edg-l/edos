use alloc::boxed::Box;
use x86_64::{
    VirtAddr,
    registers::model_specific::{GsBase, KernelGsBase},
    structures::tss::TaskStateSegment,
};

use crate::{acpi::raw_current_apic_id, thread::scheduler::Scheduler};

/// Per CPU data
/// Note: try to keep it small.
#[repr(C, align(64))]
pub struct PerCpuData {
    pub user_rsp: u64,   // Offset 0 - save user stack
    pub kernel_rsp: u64, // Offset 8 - kernel stack for syscalls
    pub tss: TaskStateSegment,
    pub lapic_id: u32,
    pub scheduler: *mut Scheduler,
}

impl PerCpuData {
    pub const fn new() -> Self {
        Self {
            user_rsp: 0,
            kernel_rsp: 0,
            lapic_id: 0,
            tss: TaskStateSegment::new(),
            scheduler: core::ptr::null_mut(),
        }
    }
}

/// Returns a mutable reference to the current CPU's PerCpuData via GS base.
#[inline(always)]
pub fn get_percpu_data() -> &'static mut PerCpuData {
    // Always keep GSBase == KernelGSBase to avoid ambiguity across SWAPGS.
    let base = GsBase::read().as_u64() as *mut PerCpuData;
    unsafe { &mut *base }
}

/// Allocate and install per-CPU data for the current CPU and set GS bases.
/// Must be called once per CPU very early during bring-up (before GDT/IDT use it).
pub unsafe fn init_gs_for_this_cpu(lapic_id: u32) -> &'static mut PerCpuData {
    let percpu_ptr: *mut PerCpuData = Box::leak(Box::new(PerCpuData::new()));
    (unsafe { percpu_ptr.as_mut().unwrap() }).lapic_id = lapic_id;
    let addr = VirtAddr::new(percpu_ptr as u64);
    // Set both to the same value so `swapgs` does not change effective base
    // and `get_percpu_data()` works uniformly.
    GsBase::write(addr);
    KernelGsBase::write(addr);
    unsafe { &mut *percpu_ptr }
}

/// BSP-only: install a statically allocated PerCpuData and set GS bases.
/// Call this in early BSP boot before the heap is initialized.
pub unsafe fn init_gs_for_bsp_static() -> &'static mut PerCpuData {
    static mut BSP_PCPU: PerCpuData = PerCpuData::new();
    unsafe {
        BSP_PCPU.lapic_id = raw_current_apic_id();
    }
    let ptr: *mut PerCpuData = &raw mut BSP_PCPU;
    let addr = VirtAddr::new(ptr as u64);
    GsBase::write(addr);
    KernelGsBase::write(addr);
    unsafe { &mut *ptr }
}
