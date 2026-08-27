use acpi::{
    AcpiTables, PhysicalMapping,
    platform::{InterruptModel, interrupt::Apic},
    sdt::madt::Madt,
};
use spin::Once;
use x86_64::instructions::interrupts::without_interrupts;

use crate::{acpi::handler::AcpiHandler, boot::boot_info, println};
use raw_cpuid::CpuId;

pub mod handler;

static ACPI_TABLES: Once<AcpiTables<AcpiHandler>> = Once::new();

static APIC_INFO: Once<Apic> = Once::new();

pub fn init_acpi() {
    acpi_tables();
    println!("Acpi tables initialized");
    apic_info();
}

pub fn acpi_tables() -> &'static AcpiTables<AcpiHandler> {
    ACPI_TABLES.call_once(|| {
        let info = boot_info();

        // A machine whose firmware describes no usable ACPI tables cannot be
        // brought up at all: the APIC, the HPET and the PCI enumeration all
        // read them.
        //
        // SAFETY: `info.rdsp` is the RSDP address Limine reported, so it names
        // a firmware structure the bootloader found and left mapped, and the
        // crate validates the signature and checksum before following it. The
        // tables it walks from there are reached through `AcpiHandler`, which
        // maps each one before reading it.
        unsafe { AcpiTables::from_rsdp(AcpiHandler, info.rdsp).expect("failed to get acpi tables") }
    })
}

pub fn acpi_madt() -> PhysicalMapping<AcpiHandler, Madt> {
    let tables = acpi_tables();

    tables.find_table::<Madt>().expect("edos needs a MADT")
}

pub fn apic_info() -> &'static Apic {
    APIC_INFO.call_once(|| {
        // The processor info alongside the model describes the MADT's view of
        // the APs; the ones this kernel starts come from Limine's MP response
        // instead.
        let (interrupt_model, _) = InterruptModel::new(acpi_tables()).expect("interrupt model");

        match interrupt_model {
            InterruptModel::Apic(apic) => apic,
            _ => panic!("edos needs an APIC interrupt model"),
        }
    })
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
