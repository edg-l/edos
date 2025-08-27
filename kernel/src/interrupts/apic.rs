use acpi::platform::InterruptModel;
use spin::{Mutex, Once};
use x2apic::{ioapic::{IoApic, IrqFlags, IrqMode, RedirectionTableEntry}, lapic::{xapic_base, LocalApic, LocalApicBuilder}};
use x86_64::{structures::paging::{mapper::MapToError, Size4KiB}, PhysAddr};

use crate::{acpi::{acpi_madt, acpi_tables, apic_info, processor_info}, interrupts::InterruptIndex, memory::mapper::get_virt_addr};


// TODO: refactor for more lapics?

pub fn get_lapic() -> LocalApic {
    let apic_physical_address = PhysAddr::new(unsafe { xapic_base() });
    let apic_virtual_address = get_virt_addr(apic_physical_address);

    LocalApicBuilder::new()
        .timer_vector(InterruptIndex::Timer as usize)
        .error_vector(InterruptIndex::Error as usize)
        .spurious_vector(InterruptIndex::Spurious as usize)
        .set_xapic_base(apic_virtual_address.as_u64())
        .build()
        .unwrap_or_else(|err| panic!("{}", err))
}


pub fn init() {

    let lapic_id = processor_info().boot_processor.local_apic_id;
    let local_apic_address = acpi_madt().get().local_apic_address;


    let apic_info = apic_info();

    if apic_info.also_has_legacy_pics {
        // disable_old_pic();
    }

    // First enable LAPIC, especially de spurious vector
    unsafe {
        //enable_apic().expect("failed to init APIC");
        enable_io_apic(apic_info, lapic_id).expect("failed to set up io apic");
    }
}


/// # Safety
/// Should only be called once.
pub unsafe fn enable_io_apic(
    info: &acpi::platform::interrupt::Apic,
    lapic_id: u32,
) -> Result<(), MapToError<Size4KiB>> {
    #[allow(clippy::never_loop)]
    for io_apic_info in info.io_apics.iter() {
        unsafe {
            let apic_physical_address = PhysAddr::new(io_apic_info.address as u64);

            let apic_virtual_address = get_virt_addr(apic_physical_address);

            let mut ioapic = IoApic::new(apic_virtual_address.as_u64());

            ioapic.init(io_apic_info.global_system_interrupt_base as u8);

            // TODO: should check
            // info.interrupt_source_overrides
            // to see if irq 1 is overriden

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

            // Set up AHCI interrupt (use IRQ 11 which matches the PCI configuration)
            let ahci_irq = 11u8;
            {
                let mut entry = RedirectionTableEntry::default();
                entry.set_mode(IrqMode::Fixed);
                entry.set_flags(IrqFlags::MASKED);
                entry.set_dest(lapic_id as u8);
                entry.set_vector(InterruptIndex::Ahci.as_u8());
                ioapic.set_table_entry(ahci_irq, entry);
                ioapic.enable_irq(ahci_irq);

                crate::serial_println!("APIC: Enabled AHCI interrupt on IRQ {}", ahci_irq);
            }
        }
        break; // just one apic now
    }

    Ok(())
}
