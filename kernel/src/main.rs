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
    fs::evict::init_evict_kthread();

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
    memory::verify_kernel_no_phys_aliasing();

    queue_spawn_kthread_named(
        "block_writeback",
        fs::writeback::writeback_thread as *const () as u64,
    );
    queue_spawn_kthread_named(
        "journal_committer",
        fs::journal::committer::committer_thread as *const () as u64,
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

/// Per-binary load request handed to a `boot-load` kthread. Owned via
/// `Box::into_raw` and reclaimed by the kthread on entry.
struct BootLoadRequest {
    name: alloc::string::String,
    argv0: alloc::vec::Vec<u8>,
    root: Path,
    result_tx: Arc<Mailbox<Arc<Thread>, ()>>,
}

/// Boot loader kthread: builds a user `Thread` (ELF load + eager-faulted
/// reloc pages) for one binary and ships it back via mailbox. Multiple
/// instances run concurrently so AHCI NCQ overlaps the per-inode page
/// fills across binaries — restores the parallelism that disappeared
/// when the loader migrated from `Vec<u8>` to file-backed mmap.
extern "C" fn boot_load_thread(arg: *mut u8) -> ! {
    let req: Box<BootLoadRequest> = unsafe { Box::from_raw(arg as *mut BootLoadRequest) };
    let path = req.root.join(&req.name).normalize();
    let inode = fs::api::resolve_inode(&path)
        .unwrap_or_else(|e| panic!("boot-load resolve_inode {}: {e:?}", req.name));
    let env: [&[u8]; 3] = [b"PATH=/bin", b"HOME=/", b"PWD=/"];
    let thread = Thread::new_user(
        inode,
        &path,
        Some(req.name.clone()),
        &[&req.argv0],
        &env,
        0,
        0,
        req.root.clone(),
    )
    .unwrap_or_else(|e| panic!("boot-load Thread::new_user {}: {e:?}", req.name));
    req.result_tx.send(thread);
    kthread_exit(0)
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

    log!("Spawning user threads via page-cache loader (parallel)");

    // Fan out one boot-load kthread per binary. Each kthread builds a
    // user Thread (ELF load + reloc page-faults) independently, so AHCI
    // NCQ can overlap the per-inode page fills across all three binaries.
    let binaries: [(&str, &[u8]); 3] = [
        ("bin/edos-wm", b"edos-wm"),
        ("bin/edos-taskbar", b"edos-taskbar"),
        ("bin/edos-terminal", b"edos-terminal"),
    ];

    let mailboxes: alloc::vec::Vec<(&str, Arc<Mailbox<Arc<Thread>, ()>>)> = binaries
        .iter()
        .map(|(name, argv0)| {
            let tx = Arc::new(Mailbox::with_capacity(1));
            let req = Box::new(BootLoadRequest {
                name: (*name).to_string(),
                argv0: argv0.to_vec(),
                root: root.clone(),
                result_tx: tx.clone(),
            });
            queue_spawn_kthread_named_arg(
                "boot-load",
                boot_load_thread as *const () as u64,
                Box::into_raw(req) as *mut u8,
            );
            (*name, tx)
        })
        .collect();

    // Collect built threads in spawn order and queue them on the scheduler.
    for (name, tx) in mailboxes {
        let mut req = tx.recv();
        let thread = req.payload.take().unwrap();
        let tid = queue_spawn_thread(thread.clone());
        log!(
            "Spawned {} tid={} cpu={}",
            name,
            tid.0,
            thread.cpu.load(core::sync::atomic::Ordering::Relaxed)
        );
    }

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
