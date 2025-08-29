use acpi::{
    AcpiTables, PhysicalMapping,
    platform::{InterruptModel, ProcessorInfo, interrupt::Apic},
    sdt::madt::Madt,
};
use spin::Once;

use crate::{acpi::handler::AcpiHandler, boot::boot_info, serial_println};

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

    serial_println!("Acpi tables initialized");

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
