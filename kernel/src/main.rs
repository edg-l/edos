#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use core::{arch::asm, time::Duration};

use x86_64::{VirtAddr, instructions::hlt};

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::init_heap,
    apic::{get_lapic, set_apic_timer_and_enable},
    boot::boot_info,
    memory::{frame_allocator::init_frame_allocator, mapper::memory_mapper},
    thread::util::queue_spawn_kthread,
    timer::{get_timer_calibration, init_boot_time, uptime_us},
};

mod acpi;
mod allocator;
mod apic;
mod boot;
mod drivers;
mod gdt;
mod graphics;
mod interrupts;
mod memory;
mod serial;
mod test;
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
    serial_println!("Initializing hpet");
    drivers::hpet::driver::init();
    serial_println!("Calibrating timer");
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

    // Init scheduler
    thread::scheduler::init();
    drivers::init_drivers();

    queue_spawn_kthread(test::thread_1);
    queue_spawn_kthread(test::thread_2);
    queue_spawn_kthread(test::thread_kb_listener);
    queue_spawn_kthread(graphics::render_thread);

    // Enable apic timer, every 1 second
    set_apic_timer_and_enable(Duration::from_millis(10));

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
