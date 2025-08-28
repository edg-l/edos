use core::sync::atomic::AtomicU64;

use x2apic::{
    ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry},
    lapic::{LocalApic, LocalApicBuilder},
};
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{PageTableFlags, Size4KiB, mapper::MapToError},
};

use crate::{
    acpi::{acpi_madt, apic_info},
    apic::{LAPIC, current_apic_id},
    interrupts::InterruptIndex,
    memory::{APIC_BASE, IOAPIC_BASE, mapper::memory_mapper},
    serial_println,
};

pub fn init() {
    let local_apic_address = acpi_madt().get().local_apic_address;

    // Map the APIC base
    {
        let mut mapper = memory_mapper();
        mapper
            .map_address(
                APIC_BASE,
                PhysAddr::new(local_apic_address as u64),
                PageTableFlags::PRESENT
                    | PageTableFlags::WRITABLE
                    | PageTableFlags::NO_CACHE
                    | PageTableFlags::WRITE_THROUGH
                    | PageTableFlags::NO_EXECUTE
                    | PageTableFlags::GLOBAL,
            )
            .unwrap();
    }

    let id = current_apic_id();
    serial_println!("Current LAPIC id: {}", id);

    let apic_info = apic_info();

    if apic_info.also_has_legacy_pics {
        serial_println!("need to disable old pic");
        unsafe {
            let mut p = pic8259::ChainedPics::new(32, 64);
            p.initialize();
            p.disable();
        }
    }

    unsafe {
        serial_println!("Enabling apic");
        LAPIC.read(); // reading enables it.
        serial_println!("Enabling io apic");
        enable_io_apic(apic_info, id).expect("failed to set up io apic");
    }
}

pub(super) unsafe fn enable_apic() -> LocalApic {
    let mut lapic = LocalApicBuilder::new()
        .timer_vector(InterruptIndex::Timer as usize)
        .error_vector(InterruptIndex::Error as usize)
        .spurious_vector(InterruptIndex::Spurious as usize)
        .set_xapic_base(APIC_BASE.as_u64())
        .build()
        .unwrap_or_else(|err| panic!("{}", err));

    unsafe {
        lapic.enable();
    }

    lapic
}

/// # Safety
/// Should only be called once.
unsafe fn enable_io_apic(
    info: &acpi::platform::interrupt::Apic,
    lapic_id: u32,
) -> Result<(), MapToError<Size4KiB>> {
    static IOAPIC_BASE_CURRENT: AtomicU64 = AtomicU64::new(IOAPIC_BASE.as_u64());
    let mut mapper = memory_mapper();

    serial_println!("I/O apics {:#?}", info.io_apics);

    #[allow(clippy::never_loop)]
    for io_apic_info in info.io_apics.iter() {
        serial_println!("Initializing i/o apic with id: {}", io_apic_info.id);
        unsafe {
            let apic_physical_address = PhysAddr::new(io_apic_info.address as u64);

            let ioapic_virt_addr =
                IOAPIC_BASE_CURRENT.fetch_add(4096, core::sync::atomic::Ordering::Relaxed);

            mapper
                .map_address(
                    VirtAddr::new_truncate(ioapic_virt_addr),
                    apic_physical_address,
                    PageTableFlags::PRESENT
                        | PageTableFlags::WRITABLE
                        | PageTableFlags::NO_CACHE
                        | PageTableFlags::WRITE_THROUGH
                        | PageTableFlags::NO_EXECUTE
                        | PageTableFlags::GLOBAL,
                )
                .unwrap();

            let mut ioapic = IoApic::new(ioapic_virt_addr);

            ioapic.init(io_apic_info.global_system_interrupt_base as u8);

            let mut keyboard_irq = None;
            let mut mouse_irq = None;

            for ov in info.interrupt_source_overrides.iter() {
                if ov.isa_source == 1 {
                    keyboard_irq = Some(ov.global_system_interrupt);
                }
                if ov.isa_source == 12 {
                    mouse_irq = Some(ov.global_system_interrupt);
                }
            }

            let keyboard_irq = keyboard_irq.unwrap_or(1) as u8;

            {
                let mut entry = RedirectionTableEntry::default();
                entry.set_mode(IrqMode::Fixed);
                entry.set_flags(IrqFlags::MASKED);
                entry.set_dest(lapic_id as u8); // cpu
                entry.set_vector(InterruptIndex::Keyboard.as_u8());
                ioapic.set_table_entry(keyboard_irq, entry);
                ioapic.enable_irq(keyboard_irq);
            }

            let mouse_irq = mouse_irq.unwrap_or(12) as u8;

            {
                let mut entry = RedirectionTableEntry::default();
                entry.set_mode(IrqMode::Fixed);
                entry.set_flags(IrqFlags::MASKED);
                entry.set_dest(lapic_id as u8); // cpu
                entry.set_vector(InterruptIndex::Mouse.as_u8());
                ioapic.set_table_entry(mouse_irq, entry);
                ioapic.enable_irq(mouse_irq);
            }
        }
        break; // just one apic now
    }

    Ok(())
}
