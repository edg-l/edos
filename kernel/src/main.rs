#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::fn_to_numeric_cast)]

use core::{hint::spin_loop, time::Duration};

use alloc::{boxed::Box, string::ToString, sync::Arc};
use x86_64::instructions::hlt;

use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::{enable_percpu_cache, init_heap, mark_gs_ready, print_alloc_stats},
    boot::boot_info,
    cmdline::ParsedCmdline,
    fs::{
        gpt::{FilesystemType, format_uuid},
        path::Path,
    },
    memory::frame_allocator::init_frame_allocator,
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
mod net;
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

    // Note: no guard page for the Limine boot stack. It lives inside the HHDM
    // which is mapped with 1GB huge pages -- can't unmap a single 4KB page.
    // Thread stacks created by the scheduler have their own guard pages.

    println!("Initializing heap");
    init_heap();
    enable_percpu_cache();
    memory::pat::init_pat();
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
    // Enable per-CPU cache globally only after all APs have their GS base set.
    // APs allocate (Box::new for PerCpuData) during init_gs_for_this_cpu before
    // their GS is ready, so gs_ready() must return false until all APs are up.
    mark_gs_ready();
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

    #[cfg(feature = "sched-test")]
    {
        crate::thread::sched_test::run_sched_tests();
        // In test mode, skip normal boot (drivers, userland, etc.).
        // The coordinator thread will exit QEMU when tests complete.
        loop {
            x86_64::instructions::interrupts::enable_and_hlt();
        }
    }

    #[allow(unreachable_code)]
    let now = Instant::now();

    while now.elapsed() < Duration::from_millis(100) {
        spin_loop();
    }
    //test_new();
    logs::init();
    crate::fs::block_page_cache::BlockPageCache::init();
    drivers::init_drivers();
    queue_spawn_kthread_named(
        "block_writeback",
        fs::writeback::writeback_thread as *const () as u64,
    );
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

/// Boot loader kthread: reads a binary from disk and sends it back via mailbox.
extern "C" fn boot_load_binary(arg: *mut u8) -> ! {
    struct LoadRequest {
        path: Path,
        result_tx: Arc<Mailbox<alloc::vec::Vec<u8>, ()>>,
    }

    let req = *unsafe { Box::from_raw(arg as *mut LoadRequest) };
    let size = fs::api::file_info(&req.path).unwrap().size as usize;
    let data = fs::api::read_bytes(&req.path, 0, size).unwrap();
    req.result_tx.send(data);
    kthread_exit(0);
}

pub fn mount_system_fs() -> ! {
    log!("Starting mountfs thread");
    let partitions = fs::api::list_partitions();

    log!("Got partitions");

    let cmdline = ParsedCmdline::parse_str(boot_info().cmdline);

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

        log!("Mounting root filesystem (EFS)");
        fs::api::mount_partition(
            part.device_id as usize,
            part.index as usize,
            root.clone(),
            part.filesystem.as_ref().expect("expected fs type").clone(),
        )
        .unwrap();
        log!("Root filesystem mounted");
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
    log!("Mounted devfs + procfs, mounting memfs /tmp");
    let tmp_dir = root.join("tmp").normalize();
    let _ = fs::api::create_dir(&tmp_dir);
    if let Err(err) = fs::api::mount_partition(0, 0, tmp_dir.clone(), FilesystemType::Memfs) {
        log!("Failed to mount memfs at {:?}: {err:?}", dev_dir);
    }

    let default_env: [&[u8]; 3] = [b"PATH=/bin", b"HOME=/", b"PWD=/"];

    // Parallel boot: load 3 binaries concurrently via per-inode locking + NCQ.
    log!("Loading boot binaries (parallel)");

    #[expect(unused)]
    struct LoadRequest {
        path: Path,
        result_tx: Arc<Mailbox<alloc::vec::Vec<u8>, ()>>,
    }

    let binaries: [(&str, Arc<Mailbox<alloc::vec::Vec<u8>, ()>>); 3] = [
        ("bin/edos-wm", Arc::new(Mailbox::with_capacity(1))),
        ("bin/edos-taskbar", Arc::new(Mailbox::with_capacity(1))),
        ("bin/edos-terminal", Arc::new(Mailbox::with_capacity(1))),
    ];

    for (name, tx) in &binaries {
        let req = Box::new(LoadRequest {
            path: root.join(name).normalize(),
            result_tx: tx.clone(),
        });
        queue_spawn_kthread_named_arg(
            "boot-load",
            boot_load_binary as *const () as u64,
            Box::into_raw(req) as *mut u8,
        );
    }

    // Wait for all 3 to complete.
    let wm_data = Arc::new(binaries[0].1.recv().payload.take().unwrap());
    log!("Loaded /bin/edos-wm ({} bytes)", wm_data.len());
    let taskbar_data = Arc::new(binaries[1].1.recv().payload.take().unwrap());
    log!("Loaded /bin/edos-taskbar ({} bytes)", taskbar_data.len());
    let terminal_data = Arc::new(binaries[2].1.recv().payload.take().unwrap());
    log!("Loaded /bin/edos-terminal ({} bytes)", terminal_data.len());
    log!("Spawning user threads");

    // Spawn initial user threads.
    let wm_thread = Thread::new_user(
        wm_data,
        Some("edos-wm".to_string()),
        &[b"edos-wm"],
        &default_env,
        0,
        0,
        root.clone(),
    )
    .unwrap();
    let wm_tid = queue_spawn_thread(wm_thread.clone());
    log!(
        "Spawned edos-wm tid={} cpu={}",
        wm_tid.0,
        wm_thread.cpu.load(core::sync::atomic::Ordering::Relaxed)
    );

    let taskbar_thread = Thread::new_user(
        taskbar_data,
        Some("edos-taskbar".to_string()),
        &[b"edos-taskbar"],
        &default_env,
        0,
        0,
        root.clone(),
    )
    .unwrap();
    let tb_tid = queue_spawn_thread(taskbar_thread.clone());
    log!(
        "Spawned edos-taskbar tid={} cpu={}",
        tb_tid.0,
        taskbar_thread
            .cpu
            .load(core::sync::atomic::Ordering::Relaxed)
    );

    let terminal_thread = Thread::new_user(
        terminal_data,
        Some("edos-terminal".to_string()),
        &[b"edos-terminal"],
        &default_env,
        0,
        0,
        root.clone(),
    )
    .unwrap();
    let tm_tid = queue_spawn_thread(terminal_thread.clone());
    log!(
        "Spawned edos-terminal tid={} cpu={}",
        tm_tid.0,
        terminal_thread
            .cpu
            .load(core::sync::atomic::Ordering::Relaxed)
    );

    kthread_exit(0)
}

// Programs are now loaded from /bin on the filesystem at runtime.

#[panic_handler]
fn rust_panic(info: &core::panic::PanicInfo) -> ! {
    // Note: do not add complex calls or memory read or scheduler reads, otherwise recursive faults can happen.
    crate::serial::emergency_write(b"\n!!! KERNEL PANIC !!!\n");
    println!("KERNEL PANIC:");
    println!("{info:#?}");

    #[cfg(feature = "trace")]
    crate::util::trace::dump_all_cpus();

    // Walk the frame pointer chain to print a backtrace.
    // With force-frame-pointers = true, RBP forms a linked list:
    //   [RBP] -> saved_rbp | [RBP+8] -> return_address
    const KERNEL_BASE: u64 = 0xFFFF_8000_0000_0000;
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
