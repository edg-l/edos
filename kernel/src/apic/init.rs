use alloc::boxed::Box;
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
    log,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper},
    println,
    util::per_cpu::get_percpu_data,
};

pub fn init() {
    let apic_info = apic_info();

    if apic_info.also_has_legacy_pics {
        log!("apic: disabling legacy PIC");
        // SAFETY: 32 and 64 are the vectors this kernel's IDT reserves for the
        // two PICs, so remapping onto them cannot collide with the CPU's
        // exception range, and masking every line afterwards is the only thing
        // done with them: from here on all external interrupts arrive through
        // the I/O APIC. Interrupts are still disabled this early in `main`, so
        // no line can fire between the remap and the mask.
        unsafe {
            let mut p = pic8259::ChainedPics::new(32, 64);
            p.initialize();
            p.disable();
        }
    }

    println!("Enabling apic");
    // SAFETY: `init` runs once per CPU during bring-up, before that CPU has a
    // LAPIC, which is what both calls require. The BSP reaches it from `main`
    // and each AP from its entry point, each on its own per-CPU block.
    unsafe { enable_lapic() };
    println!("Enabling io apic");
    // SAFETY: only the BSP reaches `init` with `also_has_legacy_pics` handling
    // above it, and the APs' entry point calls `enable_lapic` directly, so this
    // is the single I/O APIC setup the function requires.
    unsafe { enable_io_apic(apic_info) }.expect("failed to set up io apic");
}

/// Build the calling CPU's LAPIC and store it on that CPU's per-CPU block.
///
/// # Safety
/// The calling CPU must not already have a LAPIC recorded: this leaks a fresh
/// `LocalApic` into the per-CPU slot and overwriting it would strand the old
/// one while two `&'static mut` referred to the same register block. Call it
/// once per CPU, during that CPU's bring-up.
pub unsafe fn enable_lapic() {
    let mut lapic = LocalApicBuilder::new()
        .timer_vector(InterruptIndex::Timer as usize)
        .error_vector(InterruptIndex::Error as usize)
        .spurious_vector(InterruptIndex::Spurious as usize)
        .build()
        .unwrap_or_else(|err| panic!("{}", err));

    // SAFETY: the builder has already checked that the three vectors are in
    // range and that the CPU has an APIC, and the registers this writes belong
    // to the calling CPU alone. The vectors name IDT entries this kernel
    // installs before `apic::init` runs, so enabling cannot deliver to a gate
    // that is not there.
    unsafe {
        lapic.enable();
    }

    get_percpu_data().lapic.set(Box::leak(Box::new(lapic)));
}

/// Map the first I/O APIC the MADT lists and route the PS/2 keyboard and
/// mouse lines to the calling CPU.
///
/// # Safety
/// Call once, from the BSP, after `enable_lapic`. It programs a redirection
/// table that is global to the machine, so a second caller would race the
/// first over the same entries; and it needs a LAPIC on the calling CPU to
/// read the destination id from.
unsafe fn enable_io_apic(
    info: &acpi::platform::interrupt::Apic,
) -> Result<(), MapToError<Size4KiB>> {
    // SAFETY: the caller promises `enable_lapic` already ran on this CPU, so
    // the per-CPU slot holds a live `LocalApic` and reading its id only reads
    // this CPU's own register.
    let lapic_id = unsafe { get_lapic().id() } as u8;
    println!("I/O apics {:#?}", info.io_apics);

    #[expect(
        clippy::never_loop,
        reason = "the loop takes the first usable IO APIC and stops; ACPI may list more"
    )]
    for io_apic_info in info.io_apics.iter() {
        println!("Initializing i/o apic with id: {}", io_apic_info.id);

        let apic_physical_address = PhysAddr::new(io_apic_info.address as u64);
        let ioapic_virt_addr = get_virt_addr_from_phys_offset(apic_physical_address);

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

        // SAFETY: the match above leaves `ioapic_virt_addr` mapped PRESENT,
        // WRITABLE and NO_CACHE onto the physical address the MADT gives for
        // this I/O APIC, which is what the register window needs.
        let mut ioapic = unsafe { IoApic::new(ioapic_virt_addr.as_u64()) };

        // SAFETY: the window is mapped and this is the only I/O APIC setup in
        // the kernel, so nothing else is writing these registers. The base is
        // the GSI base the MADT reports for this same I/O APIC, so the
        // redirection entries below are numbered the way ACPI numbers them.
        unsafe { ioapic.init(io_apic_info.global_system_interrupt_base as u8) };

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

        // Absent an override, an ISA line keeps its identity-mapped GSI.
        let lines = [
            (
                keyboard_irq.unwrap_or(1) as u8,
                InterruptIndex::Keyboard.as_u8(),
            ),
            (mouse_irq.unwrap_or(12) as u8, InterruptIndex::Mouse.as_u8()),
        ];

        for (irq, vector) in lines {
            let mut entry = RedirectionTableEntry::default();
            entry.set_mode(IrqMode::Fixed);
            entry.set_flags(IrqFlags::MASKED);
            entry.set_dest(lapic_id);
            entry.set_vector(vector);
            // SAFETY: `ioapic` is initialised above, and `irq` is a GSI the
            // MADT named for a line this kernel has an IDT gate for. The entry
            // is written masked and only then unmasked, so the line cannot
            // fire against a half-written entry.
            unsafe {
                ioapic.set_table_entry(irq, entry);
                ioapic.enable_irq(irq);
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
    // SAFETY: every caller runs after `apic::init`, so the calling CPU has a
    // LAPIC and this reads only that CPU's own id register.
    entry.set_dest(unsafe { get_lapic().id() } as u8);
    entry.set_vector(vector);

    // SAFETY: `get_ioapic` returns a window onto the I/O APIC `enable_io_apic`
    // mapped and initialised. Device setup runs one driver at a time during
    // boot, and each driver claims a distinct `irq_line`, so no two callers
    // write the same redirection entry.
    unsafe {
        ioapic.set_table_entry(irq_line, entry);
        ioapic.enable_irq(irq_line);
    }

    Ok(())
}

pub fn get_ioapic() -> IoApic {
    let apic_info = apic_info();
    let address = apic_info.io_apics[0].address;
    let ioapic_virt_addr = get_virt_addr_from_phys_offset(PhysAddr::new(address as u64));
    // SAFETY: the same MADT address `enable_io_apic` mapped PRESENT and
    // NO_CACHE during boot, so the register window is live. `IoApic` is a thin
    // handle over that window rather than an owner of it, which is why handing
    // out a second one is not an aliasing problem; what callers must not do is
    // write the same redirection entry from two of them.
    unsafe { IoApic::new(ioapic_virt_addr.as_u64()) }
}
