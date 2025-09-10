#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::fn_to_numeric_cast)]

use core::{arch::asm, time::Duration};

use alloc::string::ToString;
use x86_64::{VirtAddr, instructions::hlt};

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::init_heap,
    apic::set_apic_timer_and_enable,
    boot::boot_info,
    memory::{frame_allocator::init_frame_allocator, mapper::memory_mapper},
    thread::{user::UserThread, util::queue_spawn_thread},
    timer::{get_timer_calibration, init_boot_time, uptime_us},
};

mod acpi;
mod allocator;
mod apic;
mod boot;
mod drivers;
mod fs;
mod gdt;
mod graphics;
mod interrupts;
mod loader;
mod memory;
mod serial;
mod syscalls;
mod thread;
mod timer;
mod util;

extern crate alloc;

fn init() {
    let info = boot_info();
    println!("Initializing frame allocator");
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

    println!("Initializing heap");
    init_heap();
    println!("Initializing acpi tables");
    init_acpi();
    println!("Initializing gdt");
    gdt::init_current_cpu();
    println!("Initializing idt");
    interrupts::init_current_cpu();
    println!("Initializing apic");
    apic::init();
    println!("Initializing hpet");
    drivers::hpet::driver::init();
    println!("Calibrating timer");
    get_timer_calibration();
    init_boot_time();
    unsafe { syscalls::setup_syscall() };
    println!("Init done");
}

fn main() -> ! {
    println!("Booting...");
    init();

    let madt = acpi_madt();
    println!(
        "Found MADT:\n{:p}",
        madt.get().local_apic_address as *mut u8
    );

    let info = boot_info();
    println!("Physical offset at {:p}", info.physical_memory_offset);

    let uptime = uptime_us();

    println!("Uptime us: {uptime}");

    // Init scheduler
    thread::scheduler::init();
    drivers::init_drivers();
    fs::init();

    queue_spawn_thread({
        let mut thread = UserThread::new(TERMINAL_PROGRAM).unwrap();
        thread.id.name = Some("terminal".to_string().into());
        thread
    });

    // Enable apic timer
    set_apic_timer_and_enable(Duration::from_millis(5));

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

pub const TERMINAL_PROGRAM: &[u8] = include_bytes!("../../programs/out/terminal");

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    println!("KERNEL PANIC:");
    println!("{info:#?}");
    loop {
        hlt();
    }
}
