#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::fn_to_numeric_cast)]

use core::{arch::asm, hint::spin_loop, time::Duration};

use alloc::{boxed::Box, string::ToString, sync::Arc};
use x86_64::{
    VirtAddr,
    instructions::{hlt, interrupts::without_interrupts},
};

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::{init_heap, print_alloc_stats},
    boot::boot_info,
    cmdline::ParsedCmdline,
    fs::{
        gpt::{FilesystemType, format_uuid},
        path::Path,
    },
    memory::{frame_allocator::init_frame_allocator, mapper::memory_mapper},
    thread::{
        mailbox::Mailbox,
        scheduler::sched,
        thread::Thread,
        util::{
            kthread_exit, queue_spawn_kthread_named, queue_spawn_kthread_named_arg,
            queue_spawn_thread,
        },
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
mod window;

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
    thread::scheduler::init_reaper();

    let now = Instant::now();

    while now.elapsed() < Duration::from_millis(100) {
        spin_loop();
    }
    //test_new();
    logs::init();
    drivers::init_drivers();
    fs::init();
    window::init();

    queue_spawn_kthread_named("system-mount", mount_system_fs as *const () as u64);

    print_alloc_stats();

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

pub fn test_new() -> ! {
    log!("Spawning test thread");

    let mb: Arc<Mailbox<u64, u64>> = Arc::new(Mailbox::new());
    queue_spawn_kthread_named_arg(
        "test",
        test_thread as *const () as u64,
        Box::into_raw(Box::new(mb.clone())).cast(),
    );
    queue_spawn_kthread_named_arg(
        "test2",
        test_thread2 as *const () as u64,
        Box::into_raw(Box::new(mb.clone())).cast(),
    );

    queue_spawn_kthread_named_arg(
        "test3",
        test_thread3 as *const () as u64,
        Box::into_raw(Box::new(mb.clone())).cast(),
    );

    x86_64::instructions::interrupts::enable_and_hlt();

    loop {
        hlt();
    }
}

extern "C" fn test_thread(arg: *mut Arc<Mailbox<u64, u64>>) -> ! {
    let mb = *unsafe { Box::from_raw(arg) };
    log!("test: Spawned test thread, waiting requests");
    loop {
        log!("test: WAITING FOR REQUEST");
        let mut req = mb.recv();
        let num = req.payload.take().unwrap();
        log!("test: Got request {}", num);
        req.reply(num);
    }
}

extern "C" fn test_thread2(arg: *mut Arc<Mailbox<u64, u64>>) -> ! {
    let mb = *unsafe { Box::from_raw(arg) };
    log!("test2: Spawned test thread2");
    let sched = sched();
    let mut counter = 0;
    loop {
        log!("test2: Sending request");
        let res = mb.send(counter);

        log!("test2: Waiting for answer");
        let c = res.wait();
        log!("test2: Got {c} expected {counter}");
        counter += 1;

        sched.thread_sleep(Duration::from_millis(1000));
    }
}

extern "C" fn test_thread3(arg: *mut Arc<Mailbox<u64, u64>>) -> ! {
    let mb = *unsafe { Box::from_raw(arg) };
    log!("test3: Spawned test thread3");
    let sched = sched();
    let mut counter = 100;
    loop {
        log!("test3: Sending request");
        let res = mb.send(counter);

        log!("test3: Waiting for answer");
        let c = res.wait();
        log!("test3: Got {c} expected {counter}");
        counter += 1;

        sched.thread_sleep(Duration::from_millis(2000));
    }
}

pub fn mount_system_fs() -> ! {
    log!("Starting mountfs thread");
    let partitions = fs::api::list_partitions();

    log!("Got partitions");

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

    log!("Mounting procfs /proc");
    let proc_dir = root.join("proc").normalize();
    let _ = fs::api::create_dir(&proc_dir);
    if let Err(err) = fs::api::mount_partition(0, 0, proc_dir.clone(), FilesystemType::Procfs) {
        log!("Failed to mount procfs at {:?}: {err:?}", proc_dir);
    }

    // TODO: add support for fstab someday.
    log!("Mounting memfs /tmp");
    let tmp_dir = root.join("tmp").normalize();
    let _ = fs::api::create_dir(&tmp_dir);
    if let Err(err) = fs::api::mount_partition(0, 0, tmp_dir.clone(), FilesystemType::Memfs) {
        log!("Failed to mount memfs at {:?}: {err:?}", dev_dir);
    }

    without_interrupts(|| {
        let wm_path = root.join("bin/edos-wm").normalize();
        let wm_argv: [&[u8]; 1] = [b"edos-wm"];
        let user_thread = Thread::new_user_from_path(
            &wm_path,
            Some("edos-wm".to_string()),
            &wm_argv,
            0,
            0,
            root.clone(),
        )
        .unwrap();
        queue_spawn_thread(user_thread);

        let wintest_path = root.join("bin/wintest").normalize();
        let wintest_argv: [&[u8]; 1] = [b"wintest"];
        let wintest_thread = Thread::new_user_from_path(
            &wintest_path,
            Some("wintest".to_string()),
            &wintest_argv,
            0,
            0,
            root.clone(),
        )
        .unwrap();
        queue_spawn_thread(wintest_thread);

        let taskbar_path = root.join("bin/edos-taskbar").normalize();
        let taskbar_argv: [&[u8]; 1] = [b"edos-taskbar"];
        let taskbar_thread = Thread::new_user_from_path(
            &taskbar_path,
            Some("edos-taskbar".to_string()),
            &taskbar_argv,
            0,
            0,
            root.clone(),
        )
        .unwrap();
        queue_spawn_thread(taskbar_thread);

        let terminal_path = root.join("bin/edos-terminal").normalize();
        let terminal_argv: [&[u8]; 1] = [b"edos-terminal"];
        let terminal_thread = Thread::new_user_from_path(
            &terminal_path,
            Some("edos-terminal".to_string()),
            &terminal_argv,
            0,
            0,
            root.clone(),
        )
        .unwrap();
        queue_spawn_thread(terminal_thread);
    });

    kthread_exit(0)
}

// Programs are now loaded from /bin on the filesystem at runtime.

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    // Note: do not add complex calls or memory read or scheduler reads, otherwise recursive faults can happen.
    println!("KERNEL PANIC:");
    println!("{info:#?}");

    #[cfg(feature = "trace")]
    crate::util::trace::dump_all_cpus();

    // Walk the frame pointer chain to print a backtrace.
    // With force-frame-pointers = true, RBP forms a linked list:
    //   [RBP] -> saved_rbp | [RBP+8] -> return_address
    const KERNEL_BASE: u64 = 0xFFFF_FFFF_8000_0000;
    const MAX_FRAMES: usize = 32;

    let mut rbp: u64;
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) rbp) };

    println!("Backtrace:");
    for i in 0..MAX_FRAMES {
        if rbp == 0 || rbp < KERNEL_BASE {
            break;
        }
        let frame_ptr = rbp as *const u64;
        // Validate the pointer is in kernel space and aligned
        if (frame_ptr as u64) < KERNEL_BASE || !frame_ptr.is_aligned() {
            break;
        }
        let ret_addr = unsafe { *frame_ptr.add(1) };
        if ret_addr == 0 {
            break;
        }
        println!("  #{i:>2}: {ret_addr:#018x}");
        let next_rbp = unsafe { *frame_ptr };
        if next_rbp <= rbp {
            break; // Stack must grow downward; prevent infinite loops
        }
        rbp = next_rbp;
    }

    loop {
        hlt();
    }
}
