#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]
#![allow(clippy::fn_to_numeric_cast)]

use core::{hint::spin_loop, time::Duration};

use alloc::{boxed::Box, string::ToString, sync::Arc};
use x86_64::instructions::hlt;

use crate::thread::scheduler::thread_sleep;
use crate::{
    acpi::{acpi_madt, init_acpi},
    allocator::{enable_percpu_cache, init_heap, mark_gs_ready, print_alloc_stats},
    boot::boot_info,
    cmdline::ParsedCmdline,
    drivers::ramdisk::RAMDISK_DEVICE_ID,
    fs::{
        gpt::{FilesystemType, Partition, format_uuid},
        path::Path,
    },
    memory::frame_allocator::init_frame_allocator,
    thread::{
        mailbox::Mailbox,
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
mod debug;
mod drivers;
mod fs;
mod gdt;
mod graphics;
mod interrupts;
mod loader;
mod logs;
mod memory;
mod net;
mod power;
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
    // Anchors the TSC to the HPET, so it must run before the first Instant.
    timer::init_monotonic_clock();
    init_boot_time();
    // Pins the RTC reading to the monotonic counter; every later wall-clock
    // answer derives from this one sample.
    timer::init_wall_clock();
    // Before the APs, so each comes up with the bits already set and only has
    // to enable PGE for itself.
    memory::mark_kernel_mappings_global();
    memory::enable_pge();
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

    // After init: parsing allocates, and the frame allocator comes up in there.
    // Still ahead of every `log_debug!` site, which are all past this point.
    let cmdline = ParsedCmdline::parse_str(boot_info().cmdline);
    let debug_logging = cmdline
        .other_params
        .iter()
        .any(|(k, v)| k == "loglevel" && v.as_deref() == Some("debug"));
    logs::set_debug_logging(debug_logging);

    if let Some(ms) = cmdline.other_params.iter().find_map(|(k, v)| {
        (k == "ahci_ncq_timeout_ms")
            .then_some(v.as_deref())
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
    }) {
        drivers::ahci::watchdog::set_ncq_timeout_ms(ms);
        println!("ahci: NCQ watchdog timeout set to {ms} ms");
    }

    if let Some(ms) = cmdline.other_params.iter().find_map(|(k, v)| {
        (k == "nvme_timeout_ms")
            .then_some(v.as_deref())
            .flatten()
            .and_then(|v| v.parse::<u64>().ok())
    }) {
        drivers::nvme::watchdog::set_nvme_timeout_ms(ms);
        println!("nvme: watchdog timeout set to {ms} ms");
    }

    // Exercises the NVMe read path from `nvme_driver_main` once controller
    // bring-up finishes, ahead of the block-io registration that would
    // otherwise be the only way to reach it.
    if cmdline
        .other_params
        .iter()
        .any(|(k, _)| k == "nvme_probe_read")
    {
        drivers::nvme::set_probe_read(true);
    }

    // Regression gate for the cancel-time `BlockBuffer` use-after-free: see
    // `block_orphan_test_thread` below. Parsed here, spawned once a device is
    // registered to submit against.
    #[cfg(feature = "fault-inject")]
    let block_orphan_test = cmdline
        .other_params
        .iter()
        .any(|(k, _)| k == "block_orphan_test");

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

    #[cfg(feature = "lock-order-self-test")]
    debug::lock_order_self_test::spawn_self_test();

    #[cfg(feature = "lock-order-self-test-inversion")]
    debug::lock_order_self_test::spawn_inversion_test();

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

    #[cfg(feature = "fault-inject")]
    if block_orphan_test {
        queue_spawn_kthread_named(
            "block-orphan-test",
            block_orphan_test_thread as *const () as u64,
        );
    }

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

/// Regression gate for the cancel-time `BlockBuffer` use-after-free: builds
/// a buffer over this thread's own stack through
/// `BlockBuffer::reaped_by_submitter`, leaks it so nothing can discharge the
/// promise, and exits. A debug kernel's per-thread borrowed-DMA counter
/// catches it at `thread_exit`; a release kernel does not, and the stack goes
/// back on the reuse queue with the buffer still outstanding.
///
/// The leak stands in for a driver op that still holds the buffer, and it is
/// what makes this deterministic. Submitting a real read and racing the
/// device is not: the completion path drops the op, and with it the buffer,
/// so whether the counter is still non-zero at `thread_exit` depends on
/// whether the disk beat the exit. Measured both ways on the same build.
///
/// This demonstrates the detection, not the corruption. Showing the
/// use-after-free itself would need a poisoned-pattern canary written into
/// the stack and checked by whichever thread reuses it.
#[cfg(feature = "fault-inject")]
pub fn block_orphan_test_thread() -> ! {
    let mut buf = [0u8; 512];
    // SAFETY: deliberately not upheld. This buffer's promise is that the
    // submitting thread reaps before it can die, and this thread exits
    // holding it, which is the fault being injected.
    let borrowed = unsafe {
        crate::drivers::block_io::BlockBuffer::reaped_by_submitter(buf.as_mut_ptr(), buf.len())
    };
    log!("block-orphan-test: exiting with a borrowed buffer outstanding");
    core::mem::forget(borrowed);
    crate::thread::scheduler::thread_exit(0);
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
    let mut counter = 0;
    loop {
        log!("test2: Sending request");
        let res = mb.send(counter);

        log!("test2: Waiting for answer");
        let c = res.wait();
        log!("test2: Got {c} expected {counter}");
        counter += 1;

        thread_sleep(Duration::from_millis(1000));
    }
}

extern "C" fn test_thread3(arg: *mut Arc<Mailbox<u64, u64>>) -> ! {
    let mb = *unsafe { Box::from_raw(arg) };
    log!("test3: Spawned test thread3");
    let mut counter = 100;
    loop {
        log!("test3: Sending request");
        let res = mb.send(counter);

        log!("test3: Waiting for answer");
        let c = res.wait();
        log!("test3: Got {c} expected {counter}");
        counter += 1;

        thread_sleep(Duration::from_millis(2000));
    }
}

/// Per-binary load request handed to a `boot-load` kthread. Owned via
/// `Box::into_raw` and reclaimed by the kthread on entry.
struct BootLoadRequest {
    name: alloc::string::String,
    argv0: alloc::vec::Vec<u8>,
    root: Path,
    result_tx: Arc<Mailbox<Option<Arc<Thread>>, ()>>,
}

/// Boot loader kthread: builds a user `Thread` (ELF load + eager-faulted
/// reloc pages) for one binary and ships it back via mailbox. Multiple
/// instances run concurrently so AHCI NCQ overlaps the per-inode page
/// fills across binaries — restores the parallelism that disappeared
/// when the loader migrated from `Vec<u8>` to file-backed mmap.
extern "C" fn boot_load_thread(arg: *mut u8) -> ! {
    let req: Box<BootLoadRequest> = unsafe { Box::from_raw(arg as *mut BootLoadRequest) };
    let path = req.root.join(&req.name).normalize();

    // A binary that will not load is a broken filesystem, not a broken kernel.
    // Report it and leave the machine up: the serial console and dmesg are far
    // more use for diagnosing this than a panic is.
    let thread = fs::api::resolve_inode(&path)
        .map_err(|e| alloc::format!("resolve_inode: {e:?}"))
        .and_then(|inode| {
            let env: [&[u8]; 3] = [b"PATH=/bin", b"HOME=/", b"PWD=/"];
            Thread::new_user(
                inode,
                &path,
                Some(req.name.clone()),
                &[&req.argv0],
                &env,
                0,
                0,
                req.root.clone(),
            )
            .map_err(|e| alloc::format!("load: {e:?}"))
        });

    match thread {
        Ok(thread) => {
            req.result_tx.send(Some(thread));
        }
        Err(reason) => {
            log!("boot-load {}: {reason}", req.name);
            req.result_tx.send(None);
        }
    }
    kthread_exit(0)
}

/// Pick the partition to mount as root, or `None` to fall back to memfs.
///
/// `root=UUID=<guid>` selects by partition GUID. An installed disk and the
/// live image the machine booted from carry the same GUID, so a match on a
/// real disk wins over the ramdisk; `root=live` forces the ramdisk. Nothing
/// here is positional: a UUID that matches nothing mounts nothing, rather than
/// falling back to whichever partition happened to enumerate first.
fn select_root_partition(partitions: &[Partition], root: Option<&str>) -> Option<usize> {
    let Some(root) = root.map(str::trim) else {
        println!("Empty cmdline, using memfs as root.");
        return None;
    };

    let is_live = |p: &Partition| p.device_id == RAMDISK_DEVICE_ID;

    if root == "live" {
        let found = partitions.iter().position(is_live);
        if found.is_none() {
            log!("root=live, but no live root image was loaded");
        }
        return found;
    }

    let candidates = match root.split_once('=') {
        Some(("UUID", value)) => partitions
            .iter()
            .enumerate()
            .filter(|(_, p)| format_uuid(&p.unique_partition_guid).eq_ignore_ascii_case(value))
            .map(|(i, _)| i)
            .collect(),
        _ => {
            log!("Unsupported root {root:?}, only UUID= and live are supported");
            alloc::vec::Vec::new()
        }
    };

    // An installed disk wins over the live image that booted it.
    let chosen = candidates
        .iter()
        .copied()
        .find(|&i| !is_live(&partitions[i]))
        .or_else(|| candidates.first().copied());

    match chosen {
        Some(i) => log!(
            "Root partition: {} on device {}",
            root,
            partitions[i].device_id
        ),
        None => {
            log!("No partition matches {root}. Partitions seen:");
            for (i, p) in partitions.iter().enumerate() {
                log!(
                    "  [{i}] device {} index {} guid {} fs {:?} name {:?}",
                    p.device_id,
                    p.index,
                    format_uuid(&p.unique_partition_guid),
                    p.filesystem,
                    p.name
                );
            }
        }
    }

    chosen
}

pub fn mount_system_fs() -> ! {
    log!("Starting mountfs thread");
    let partitions = match fs::api::list_partitions() {
        Ok(parts) => parts,
        Err(e) => panic!("cannot enumerate partitions, no root filesystem: {e}"),
    };

    log!("Got partitions");

    let cmdline = ParsedCmdline::parse_str(boot_info().cmdline);

    let part_idx = select_root_partition(&partitions, cmdline.root.as_deref());

    let root = Path::root();
    if let Some(part) = part_idx.map(|i| &partitions[i]) {
        log!("Partition name {:?}", part.name);

        log!("Mounting root filesystem (EFS)");
        fs::api::mount_partition(
            part.device_id as usize,
            part.index,
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

    // One userspace process is started: init. What else runs, and what happens
    // when it dies, is init's policy rather than the kernel's.
    let name = "bin/edos-init";
    let tx: Arc<Mailbox<Option<Arc<Thread>>, ()>> = Arc::new(Mailbox::with_capacity(1));
    let req = Box::new(BootLoadRequest {
        name: name.to_string(),
        argv0: b"edos-init".to_vec(),
        root: root.clone(),
        result_tx: tx.clone(),
    });
    queue_spawn_kthread_named_arg(
        "boot-load",
        boot_load_thread as *const () as u64,
        Box::into_raw(req) as *mut u8,
    );

    let mut req = tx.recv();
    match req.payload.take() {
        Some(Some(thread)) => {
            let tid = queue_spawn_thread(thread.clone());
            crate::thread::thread::set_init_pid(tid.0);
            log!(
                "Spawned {} tid={} cpu={}",
                name,
                tid.0,
                thread.cpu.load(core::sync::atomic::Ordering::Relaxed)
            );
        }
        _ => {
            log!("FATAL: {name} did not load; no userspace will run");
            log!("The kernel stays up: use the serial console and dmesg to diagnose.");
        }
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
