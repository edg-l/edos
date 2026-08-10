#![expect(unused)]

use acpi::{
    AcpiTables, PhysicalMapping,
    platform::{InterruptModel, ProcessorInfo, interrupt::Apic},
    sdt::madt::Madt,
};
use spin::Once;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{acpi::handler::AcpiHandler, boot::boot_info, println};
use alloc::{collections::BTreeMap, vec::Vec};
use raw_cpuid::CpuId;

pub mod handler;

static ACPI_TABLES: Once<AcpiTables<AcpiHandler>> = Once::new();

static PROCESSOR_INFO: Once<ProcessorInfo> = Once::new();
static APIC_INFO: Once<Apic> = Once::new();

pub fn init_acpi() {
    ACPI_TABLES.call_once(|| {
        let info = boot_info();

        unsafe { AcpiTables::from_rsdp(AcpiHandler, info.rdsp).expect("failed to get acpi tables") }
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

/// Returns the raw current APIC ID via CPUID topology.
pub fn raw_current_apic_id() -> u32 {
    without_interrupts(|| {
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
    })
}
