#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use core::arch::asm;

use x86_64::{VirtAddr, instructions::hlt};

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::init_heap,
    apic::LAPIC,
    boot::boot_info,
    memory::{frame_allocator::init_frame_allocator, mapper::memory_mapper},
    timer::{get_timer_calibration, init_boot_time, uptime_us},
};

mod acpi;
mod allocator;
mod apic;
mod boot;
mod gdt;
mod interrupts;
mod memory;
mod serial;
mod thread;
mod timer;
mod util;

extern crate alloc;

fn init() {
    let info = boot_info();
    serial_println!("Initializing frame allocator");
    init_frame_allocator(info.memory_map);

    {
        // Setup a kernel stack guard
        let current_sp: u64;
        unsafe {
            asm!("mov {}, rsp", out(reg) current_sp);
        }

        let stack_bottom = (current_sp & !0xfff) - (16 * 1024); // we requested 16kb stack
        let guard_page = stack_bottom - 4096; // Page just below stack

        // Unmap the guard page
        memory_mapper()
            .unmap_memory(VirtAddr::new(guard_page), 4095)
            .unwrap();
    }

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
    get_timer_calibration();
    init_boot_time();
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

    let uptime = uptime_us();

    serial_println!("Uptime us: {uptime}");

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

    {
        unsafe {
            let mut lapic = LAPIC.write();
            let timer = get_timer_calibration();
            lapic.set_timer_mode(x2apic::lapic::TimerMode::Periodic);
            lapic.set_timer_divide(x2apic::lapic::TimerDivide::Div1);
            lapic.set_timer_initial(timer.ticks_per_microsecond as u32 * 1000_000);
            lapic.enable_timer();
        }
    }

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
        serial_println!("hi from halt");
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
