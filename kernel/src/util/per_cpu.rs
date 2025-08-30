use raw_cpuid::CpuId;
use x2apic::lapic::LocalApic;
use x86_64::structures::tss::TaskStateSegment;

#[repr(C)]
pub struct PerCpuData {
    pub tss: TaskStateSegment,
    pub lapic: *mut LocalApic,
}

#[used]
#[unsafe(link_section = ".percpu")]
static mut PERCPU_TEMPLATE: PerCpuData = PerCpuData {
    lapic: core::ptr::null_mut(),
    tss: TaskStateSegment::new(),
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

    // Try extended topology first
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

    // Last resort - shouldn't happen on modern CPUs
    0
}
