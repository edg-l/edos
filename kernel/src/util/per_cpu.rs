use x86_64::structures::tss::TaskStateSegment;

use crate::acpi;
use crate::thread::scheduler::Scheduler;

/// Per cpu data
///
/// Note: try to keep it small.
#[repr(C, align(64))]
pub struct PerCpuData {
    user_rsp: u64,   // Offset 0 - save user stack
    kernel_rsp: u64, // Offset 8 - kernel stack for syscalls
    pub tss: TaskStateSegment,
    pub scheduler: *mut Scheduler,
}

#[used]
#[unsafe(link_section = ".percpu.tpl")]
static mut PERCPU_TEMPLATE: PerCpuData = PerCpuData {
    user_rsp: 0,
    kernel_rsp: 0,
    tss: TaskStateSegment::new(),
    scheduler: core::ptr::null_mut(),
};

unsafe extern "C" {
    static __percpu_start: u8;
    static __percpu_tpl_start: u8;
    static __percpu_tpl_end: u8;
    static __percpu_stride: usize; // absolute symbol from ld
}

#[inline]
fn percpu_base() -> usize {
    unsafe { &__percpu_start as *const u8 as usize }
}

#[inline]
fn percpu_stride() -> usize {
    // Prefer the absolute stride symbol if supported by your ld.
    // Fallback: compute from template bounds and align in Rust to match the script.
    unsafe {
        let s = &__percpu_stride as *const usize as usize;
        if s != 0 {
            return s;
        }
        let a = &__percpu_tpl_start as *const u8 as usize;
        let b = &__percpu_tpl_end as *const u8 as usize;
        (b - a + 63) & !63
    }
}

pub fn get_percpu_data() -> &'static mut PerCpuData {
    // Use compact CPU index mapping instead of raw APIC IDs.
    let cpu_index = acpi::current_cpu_index();
    let ptr = percpu_base() + cpu_index * percpu_stride();
    unsafe { &mut *(ptr as *mut PerCpuData) }
}

/* Early boot on each CPU: copy template into its slot once */
pub unsafe fn init_this_cpu_percpu() {
    let cpu_index = acpi::current_cpu_index();
    let dst = (percpu_base() + cpu_index * percpu_stride()) as *mut u8;
    let src = unsafe { &__percpu_tpl_start } as *const u8;
    let len = (unsafe { &__percpu_tpl_end } as *const u8 as usize) - (src as usize);
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
}
