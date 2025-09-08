#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

use core::{arch::asm, time::Duration};

use alloc::string::ToString;
use x86_64::{VirtAddr, instructions::hlt};

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::init_heap,
    apic::set_apic_timer_and_enable,
    boot::boot_info,
    drivers::ahci,
    memory::{frame_allocator::init_frame_allocator, mapper::memory_mapper},
    thread::{
        scheduler::sched,
        user::UserThread,
        util::{queue_spawn_kthread_named, queue_spawn_thread},
    },
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
mod loader;
mod memory;
mod serial;
mod syscalls;
mod thread;
mod timer;
mod util;
mod fs;

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
    gdt::init();
    println!("Initializing idt");
    interrupts::init();
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

    queue_spawn_kthread_named("test-reader", thread_test_read);
    queue_spawn_thread({
        let mut thread = UserThread::new(TERMINAL_PROGRAM).unwrap();
        thread.id.name = Some("terminal".to_string().into());
        thread
    });

    // Enable apic timer, every 1 second
    set_apic_timer_and_enable(Duration::from_millis(10));

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

pub const TERMINAL_PROGRAM: &[u8] = include_bytes!("../../programs/out/terminal");

pub fn thread_test_read() -> ! {
    let detected_devices = ahci::api::list_devices();

    // Test reading GPT header from first detected device
    if !detected_devices.is_empty() {
        use alloc::string::String;

        let mut output = String::new();

        // Get the first controller and port with a device
        if let Some(device) = detected_devices.first() {
            // Find the controller

            println!("Reading GPT header from LBA 1...");

            let gpt_buffer = ahci::api::read_sectors(device.id, 1, 1);

            if let Ok(gpt_buffer) = gpt_buffer {
                output.push_str("Successfully read GPT header\n");

                // Check for GPT signature "EFI PART"
                let signature = &gpt_buffer[0..8];

                println!("Read signature: {signature:?}");
                if signature == b"EFI PART" {
                    output.push_str("Valid GPT signature found\n");

                    // Parse some basic GPT header fields
                    let revision = u32::from_le_bytes([
                        gpt_buffer[8],
                        gpt_buffer[9],
                        gpt_buffer[10],
                        gpt_buffer[11],
                    ]);
                    let header_size = u32::from_le_bytes([
                        gpt_buffer[12],
                        gpt_buffer[13],
                        gpt_buffer[14],
                        gpt_buffer[15],
                    ]);
                    let num_partition_entries = u32::from_le_bytes([
                        gpt_buffer[80],
                        gpt_buffer[81],
                        gpt_buffer[82],
                        gpt_buffer[83],
                    ]);

                    output.push_str(&alloc::format!("GPT Revision: {:#x}\n", revision));
                    output.push_str(&alloc::format!("Header Size: {} bytes\n", header_size));
                    output.push_str(&alloc::format!(
                        "Number of partition entries: {}\n",
                        num_partition_entries
                    ));
                    output.push('\n');
                } else {
                    output.push_str("No valid GPT signature found. Raw signature bytes:\n");
                    output.push_str("Signature: ");
                    for &byte in signature {
                        output.push_str(&alloc::format!("{:02x} ", byte));
                    }
                    output.push('\n');

                    // Check if it might be MBR instead
                    if gpt_buffer[510] == 0x55 && gpt_buffer[511] == 0xAA {
                        output.push_str("This appears to be an MBR disk (boot signature found)\n");
                    } else {
                        output.push_str("Unknown partition table format\n");
                    }
                }
            }

            println!("{}", output);
            output.clear();

            println!("Reading lba 0 with 2 sectors..");

            let multi_buffer = ahci::api::read_sectors(device.id, 0, 2);

            match multi_buffer {
                Ok(multi_buffer) => {
                    output.push_str("Successfully read 2 sectors!\n");
                    output.push_str(&alloc::format!(
                        "MBR signature at end of first sector: {:02x} {:02x}\n",
                        multi_buffer[510],
                        multi_buffer[511]
                    ));
                    output.push_str(&alloc::format!(
                        "GPT signature in second sector: {:?}\n",
                        core::str::from_utf8(&multi_buffer[512..520]).unwrap_or("invalid")
                    ));
                }
                Err(e) => {
                    output.push_str(&alloc::format!(
                        "Failed to read multiple sectors: {:?}\n",
                        e
                    ));
                }
            }
        }

        println!("{}", output);
    }

    loop {
        sched().thread_park();
    }
}

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    println!("KERNEL PANIC:");
    println!("{info:#?}");
    loop {
        hlt();
    }
}
