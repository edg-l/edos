use raw_cpuid::CpuId;
use x2apic::lapic::LocalApic;
use x86_64::structures::tss::TaskStateSegment;

use crate::thread::scheduler::Scheduler;

/// Per cpu data
///
/// Note: try to keep it small.
#[repr(C)]
pub struct PerCpuData {
    pub tss: TaskStateSegment,
    // Since we use those after heap is done, we can store pointers to the heap allocations.
    pub lapic: *mut LocalApic,
    pub scheduler: *mut Scheduler,
}

#[used]
#[unsafe(link_section = ".percpu")]
static mut PERCPU_TEMPLATE: PerCpuData = PerCpuData {
    lapic: core::ptr::null_mut(),
    tss: TaskStateSegment::new(),
    scheduler: core::ptr::null_mut(),
};

unsafe extern "C" {
    static __percpu_start: u8;
    static __percpu_size: usize;
}

pub fn get_percpu_data() -> &'static mut PerCpuData {
    let cpu_id = get_current_cpu_id();
    unsafe {
        let base = &__percpu_start as *const u8 as usize;
        let offset = cpu_id * (&__percpu_size as *const usize as usize);
        &mut *((base + offset) as *mut PerCpuData)
    }
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
