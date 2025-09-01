use raw_cpuid::CpuId;
use x86_64::structures::tss::TaskStateSegment;

use crate::thread::scheduler::Scheduler;

/// Per cpu data
///
/// Note: try to keep it small.
#[repr(C, align(64))]
pub struct PerCpuData {
    pub tss: TaskStateSegment,
    pub scheduler: *mut Scheduler,
}

#[used]
#[unsafe(link_section = ".percpu.tpl")]
static mut PERCPU_TEMPLATE: PerCpuData = PerCpuData {
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
    let id = get_current_cpu_id();
    let ptr = percpu_base() + id * percpu_stride();
    unsafe { &mut *(ptr as *mut PerCpuData) }
}

/* Early boot on each CPU: copy template into its slot once */
pub unsafe fn init_this_cpu_percpu() {
    let id = get_current_cpu_id();
    let dst = (percpu_base() + id * percpu_stride()) as *mut u8;
    let src = unsafe { &__percpu_tpl_start } as *const u8;
    let len = (unsafe { &__percpu_tpl_end } as *const u8 as usize) - (src as usize);
    unsafe { core::ptr::copy_nonoverlapping(src, dst, len) };
}

pub fn get_current_cpu_id() -> usize {
    let cpuid = CpuId::new();

    if let Some(extended_topology) = cpuid.get_extended_topology_info() {
        for level in extended_topology {
            if level.level_type() == raw_cpuid::TopologyType::Core {
                return level.x2apic_id() as usize;
            }
        }
    }

    // Fallback to basic feature info
    if let Some(feature_info) = cpuid.get_feature_info() {
        return feature_info.initial_local_apic_id() as usize;
    }

    // Last resort
    0
}
