#![expect(unused)]

use acpi::{
    AcpiTables, PhysicalMapping,
    platform::{InterruptModel, ProcessorInfo, interrupt::Apic},
    sdt::madt::Madt,
};
use spin::Once;

use crate::{acpi::handler::AcpiHandler, boot::boot_info, println};
use alloc::{collections::BTreeMap, vec::Vec};
use raw_cpuid::CpuId;

mod handler;

static ACPI_TABLES: Once<AcpiTables<AcpiHandler>> = Once::new();

static PROCESSOR_INFO: Once<ProcessorInfo> = Once::new();
static APIC_INFO: Once<Apic> = Once::new();

pub fn init_acpi() {
    ACPI_TABLES.call_once(|| {
        let info = boot_info();

        unsafe {
            AcpiTables::from_rsdp(AcpiHandler::new(), info.rdsp).expect("failed to get acpi tables")
        }
    });

    println!("Acpi tables initialized");

    let (interrupt_model, processor_info) =
        InterruptModel::new(acpi_tables()).expect("interrupt model");

    APIC_INFO.call_once(|| match interrupt_model {
        InterruptModel::Unknown => unimplemented!(),
        InterruptModel::Apic(apic) => apic,
        _ => unimplemented!(),
    });

    let processor_info = processor_info.unwrap();

    PROCESSOR_INFO.call_once(|| processor_info);

    // Build CPU topology and APIC-ID → compact index map
    init_cpu_topology();
}

pub fn acpi_tables() -> &'static AcpiTables<AcpiHandler> {
    ACPI_TABLES.get().unwrap()
}

pub fn acpi_madt() -> PhysicalMapping<AcpiHandler, Madt> {
    let tables = acpi_tables();

    tables.find_table::<Madt>().expect("edos needs a MADT")
}

pub fn processor_info() -> &'static ProcessorInfo {
    PROCESSOR_INFO.get().unwrap()
}

pub fn apic_info() -> &'static Apic {
    APIC_INFO.get().unwrap()
}

static NUMBER_OF_CORES: Once<usize> = Once::new();

pub fn number_of_cores() -> usize {
    *NUMBER_OF_CORES.call_once(|| 1 + processor_info().application_processors.len())
}

// ---- CPU topology and APIC-ID mapping ----

#[derive(Debug)]
struct CpuTopology {
    // Ordered list: index 0 = BSP, then APs
    apic_ids: Vec<u32>,
    // Reverse lookup: APIC ID → compact cpu_index
    index_by_apic: BTreeMap<u32, usize>,
}

static CPU_TOPOLOGY: Once<CpuTopology> = Once::new();

fn init_cpu_topology() {
    CPU_TOPOLOGY.call_once(|| {
        let info = processor_info();
        let mut apic_ids = Vec::new();

        // Boot processor first (index 0)
        apic_ids.push(info.boot_processor.local_apic_id);

        // Then application processors in discovery order
        for ap in &info.application_processors {
            apic_ids.push(ap.local_apic_id);
        }

        let mut index_by_apic = BTreeMap::new();
        for (idx, apic_id) in apic_ids.iter().copied().enumerate() {
            index_by_apic.insert(apic_id, idx);
        }

        println!("[acpi] CPU topology: {:?}", apic_ids);

        CpuTopology {
            apic_ids,
            index_by_apic,
        }
    });
}

/// Returns the compact 0..N-1 CPU index for the given APIC ID.
pub fn cpu_index_from_apic_id(apic_id: u32) -> Option<usize> {
    CPU_TOPOLOGY
        .get()
        .and_then(|topo| topo.index_by_apic.get(&apic_id).copied())
}

/// Returns the current CPU's compact index. Falls back to 0 if unavailable.
pub fn current_cpu_index() -> usize {
    let apic_id = raw_current_apic_id();
    // Mapping may not be initialized extremely early; default to BSP.
    cpu_index_from_apic_id(apic_id).unwrap_or_default()
}

/// Returns the raw current APIC ID via CPUID topology.
pub fn raw_current_apic_id() -> u32 {
    let cpuid = CpuId::new();
    if let Some(extended_topology) = cpuid.get_extended_topology_info() {
        for level in extended_topology {
            if level.level_type() == raw_cpuid::TopologyType::Core {
                return level.x2apic_id();
            }
        }
    }
    if let Some(feature_info) = cpuid.get_feature_info() {
        return feature_info.initial_local_apic_id() as u32;
    }
    0
}
