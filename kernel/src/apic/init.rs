use x2apic::{
    ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry},
    lapic::LocalApicBuilder,
};
use x86_64::{
    PhysAddr,
    structures::paging::{
        PageTableFlags, Size4KiB,
        mapper::{MapToError, TranslateResult},
    },
};

use crate::{
    acpi::apic_info,
    apic::get_lapic,
    interrupts::InterruptIndex,
    memory::{get_virt_addr, mapper::memory_mapper},
    println,
};

pub fn init() {
    let apic_info = apic_info();

    if apic_info.also_has_legacy_pics {
        println!("need to disable old pic");
        unsafe {
            let mut p = pic8259::ChainedPics::new(32, 64);
            p.initialize();
            p.disable();
        }
    }

    unsafe {
        println!("Enabling apic");
        enable_lapic();
        println!("Enabling io apic");
        enable_io_apic(apic_info).expect("failed to set up io apic");
    }
}

pub(super) unsafe fn enable_lapic() {
    let mut lapic = LocalApicBuilder::new()
        .timer_vector(InterruptIndex::Timer as usize)
        .error_vector(InterruptIndex::Error as usize)
        .spurious_vector(InterruptIndex::Spurious as usize)
        .build()
        .unwrap_or_else(|err| panic!("{}", err));

    unsafe {
        lapic.enable();
    }
}

/// # Safety
/// Should only be called once.
unsafe fn enable_io_apic(
    info: &acpi::platform::interrupt::Apic,
) -> Result<(), MapToError<Size4KiB>> {
    let lapic_id = unsafe { get_lapic().id() };
    println!("I/O apics {:#?}", info.io_apics);

    #[allow(clippy::never_loop)]
    for io_apic_info in info.io_apics.iter() {
        println!("Initializing i/o apic with id: {}", io_apic_info.id);
        unsafe {
            let apic_physical_address = PhysAddr::new(io_apic_info.address as u64);

            let ioapic_virt_addr = get_virt_addr(apic_physical_address);

            {
                let mut mapper = memory_mapper();
                match mapper.translate(ioapic_virt_addr) {
                    TranslateResult::Mapped {
                        frame,
                        offset,
                        flags,
                    } => {
                        println!("lapic already mapped: {frame:?} {offset} {flags:?}");
                    }
                    TranslateResult::NotMapped => {
                        println!("I/O APIC not mapped, mapping");
                        if mapper
                            .map_address(
                                ioapic_virt_addr,
                                apic_physical_address,
                                PageTableFlags::PRESENT
                                    | PageTableFlags::WRITABLE
                                    | PageTableFlags::NO_CACHE
                                    | PageTableFlags::GLOBAL,
                            )
                            .is_err()
                        {
                            println!("failed to map I/O APIC, already mapped");
                        }
                    }
                    TranslateResult::InvalidFrameAddress(_) => {
                        unreachable!()
                    }
                }
            }

            let mut ioapic = IoApic::new(ioapic_virt_addr.as_u64());

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

pub fn configure_device_interrupt(irq_line: u8, vector: u8) -> Result<(), MapToError<Size4KiB>> {
    let mut ioapic = get_ioapic();

    let mut entry = RedirectionTableEntry::default();
    entry.set_mode(IrqMode::Fixed);
    entry.set_flags(IrqFlags::LEVEL_TRIGGERED); // AHCI uses level-triggered
    entry.set_dest(unsafe { get_lapic().id() } as u8);
    entry.set_vector(vector);

    unsafe {
        ioapic.set_table_entry(irq_line, entry);
        ioapic.enable_irq(irq_line);
    }

    Ok(())
}

pub fn get_ioapic() -> IoApic {
    let apic_info = apic_info();
    let address = apic_info.io_apics[0].address;
    let ioapic_virt_addr = get_virt_addr(PhysAddr::new(address as u64));
    unsafe { IoApic::new(ioapic_virt_addr.as_u64()) }
}
