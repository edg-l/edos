#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use x86_64::instructions::hlt;

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::init_heap,
    boot::boot_info,
    memory::frame_allocator::init_frame_allocator,
};

mod acpi;
mod allocator;
mod apic;
mod boot;
mod gdt;
mod interrupts;
mod memory;
mod serial;
mod util;

extern crate alloc;

fn init() {
    let info = boot_info();
    serial_println!("Initializing frame allocator");
    init_frame_allocator(info.memory_map);
    serial_println!("Initializing heap");
    init_heap();
    serial_println!("Initializing acpi tables");
    init_acpi();
    serial_println!("Initializing gdt");
    gdt::init();
    serial_println!("Initializing idt");
    interrupts::init();
    serial_println!("Initializing apic");
    apic::init();
    serial_println!("Init done");
}

fn main() -> ! {
    serial_println!("Booting...");
    init();

    let madt = acpi_madt();
    serial_println!(
        "Found MADT:\n{:p}",
        madt.get().local_apic_address as *mut u8
    );

    let info = boot_info();
    serial_println!("Physical offset at {:p}", info.physical_memory_offset);

    for i in 0..100_u64 {
        // Calculate the pixel offset using the framebuffer information we obtained above.
        // We skip `i` scanlines (pitch is provided in bytes) and add `i * 4` to skip `i` pixels forward.
        let pixel_offset = i * info.framebuffer.pitch() + i * 4;

        // Write 0xFFFFFFFF to the provided pixel offset to fill it white.
        unsafe {
            info.framebuffer
                .addr()
                .add(pixel_offset as usize)
                .cast::<u32>()
                .write(0xFFFFFFFF)
        };
    }

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    serial_println!("KERNEL PANIC:");
    serial_println!("{info:#?}");
    loop {
        hlt();
    }
}
