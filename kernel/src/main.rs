#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::fn_to_numeric_cast)]

use core::{arch::asm, time::Duration};

use alloc::string::ToString;
use x86_64::{
    VirtAddr,
    instructions::{hlt, interrupts::without_interrupts},
};

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::{init_heap, print_alloc_stats},
    apic::set_apic_timer_and_enable,
    boot::boot_info,
    cmdline::ParsedCmdline,
    fs::{
        gpt::{FilesystemType, format_uuid},
        path::Path,
    },
    memory::{frame_allocator::init_frame_allocator, mapper::memory_mapper},
    thread::{
        UserThreadInfo,
        scheduler::sched,
        thread::Thread,
        util::{kthread_exit, queue_spawn_kthread_named, queue_spawn_thread},
    },
    timer::{Instant, get_timer_calibration, init_boot_time, uptime_us},
};

mod acpi;
mod allocator;
mod apic;
mod boot;
mod cmdline;
mod drivers;
mod fs;
mod gdt;
mod graphics;
mod interrupts;
mod loader;
mod logs;
mod memory;
mod serial;
mod smp;
mod syscalls;
mod thread;
mod timer;
mod util;

extern crate alloc;

fn init() {
    let rtc_time = crate::drivers::rtc::read_rtc();
    println!(
        "Boot time: {:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        rtc_time.year,
        rtc_time.month,
        rtc_time.day,
        rtc_time.hour,
        rtc_time.minute,
        rtc_time.second
    );
    let info = boot_info();
    println!("cmdline: {:?}", info.cmdline);
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
    smp::init();
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
    // Enable apic timer
    set_apic_timer_and_enable(Duration::from_millis(5));
    test_new();
    logs::init();
    drivers::init_drivers();
    fs::init();

    queue_spawn_kthread_named("system-mount", mount_system_fs as u64);

    print_alloc_stats();

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

pub fn test_new() -> ! {
    println!("Spawning test thread");
    queue_spawn_kthread_named("test", test_thread as u64);
    queue_spawn_kthread_named("test2", test_thread2 as u64);

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

extern "C" fn test_thread() -> ! {
    println!("Spawned test thread");
    let sched = sched();
    loop {
        println!("t1");

        sched.thread_sleep(Duration::from_secs(1));
    }
}

extern "C" fn test_thread2() -> ! {
    println!("Spawned test thread");
    let sched = sched();
    loop {
        println!("t2");

        sched.thread_sleep(Duration::from_millis(500));
    }
}

pub fn mount_system_fs() -> ! {
    let partitions = fs::api::list_partitions();

    let cmdline = ParsedCmdline::parse(boot_info().cmdline);

    let mut part_idx = 0;

    if cmdline.root.is_none() {
        println!("Empty cmdline, using memfs as root.");
    } else {
        let mut keyval = cmdline.root.as_ref().unwrap().trim().split("=");
        let root_type = keyval.next();
        let root_value = keyval.next();

        if let Some(root_type) = root_type
            && let Some(root_value) = root_value
        {
            if root_type == "UUID" {
                for (i, part) in partitions.iter().enumerate() {
                    if format_uuid(&part.unique_partition_guid).eq_ignore_ascii_case(root_value) {
                        part_idx = i;
                        log!("Found root partition with uuid {}", root_value);
                        break;
                    }
                }
            } else {
                log!("Unsupported root type, only UUID is supported");
            }
        }
    }

    let root = Path::parse("/").unwrap();
    if !partitions.is_empty() && cmdline.root.is_some() {
        let part = &partitions[part_idx];
        log!("Partition name {:?}", part.name);

        fs::api::mount_partition(
            part.device_id as usize,
            part.index as usize,
            root.clone(),
            part.filesystem.as_ref().expect("expected fs type").clone(),
        )
        .unwrap();
    } else {
        log!("Mounting memfs on /");
        fs::api::mount_partition(0, 0, root.clone(), FilesystemType::Memfs).unwrap();
    }

    let dev_dir = root.join("dev").normalize();
    let _ = fs::api::create_dir(&dev_dir);
    if let Err(err) = fs::api::mount_partition(0, 0, dev_dir.clone(), FilesystemType::Devfs) {
        log!("Failed to mount devfs at {:?}: {err:?}", dev_dir);
    }

    // TODO: add support for fstab someday.
    log!("Mounting memfs /tmp");
    let tmp_dir = root.join("tmp").normalize();
    let _ = fs::api::create_dir(&tmp_dir);
    if let Err(err) = fs::api::mount_partition(0, 0, tmp_dir.clone(), FilesystemType::Memfs) {
        log!("Failed to mount memfs at {:?}: {err:?}", dev_dir);
    }

    without_interrupts(|| {
        let terminal_argv: [&[u8]; 1] = [b"terminal"];
        let user_thread = Thread::new_user(
            TERMINAL_PROGRAM,
            Some("terminal".to_string()),
            &terminal_argv,
            0,
            0,
            root.clone(),
        )
        .unwrap();
        queue_spawn_thread(user_thread);
    });

    kthread_exit(0)
}

pub const TERMINAL_PROGRAM: &[u8] = include_bytes!("../../filesystem/bin/terminal");

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    // Note: do not add complex calls or memory read or scheduler reads, otherwise recursive faults can happen.
    println!("KERNEL PANIC:");
    println!("{info:#?}");
    loop {
        hlt();
    }
}
