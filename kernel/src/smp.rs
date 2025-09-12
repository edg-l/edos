use core::time::Duration;

use limine::mp::Cpu as MpCpu;

use crate::{
    apic::{get_lapic, init::enable_lapic, set_apic_timer_and_enable},
    boot::MP_REQUEST,
    drivers::fpu,
    gdt, interrupts, log, println,
    syscalls::setup_syscall,
    thread::{
        self,
        scheduler::{sched, switch_to_kernel_page},
        util::queue_spawn_kthread_named,
    },
    util::per_cpu::init_this_cpu_percpu,
};

/// Initialize SMP using Limine's MP request: set AP entrypoints and let Limine bring them up.
pub fn init() {
    // Ensure the request is referenced so the linker keeps it.
    if let Some(resp) = MP_REQUEST.get_response() {
        let bsp_lapic = resp.bsp_lapic_id();
        for &cpu in resp.cpus() {
            // Skip the BSP; it is already running `init()` and `main()`.
            if cpu.lapic_id == bsp_lapic {
                continue;
            }

            println!("Initing: {:#?} (bsp: {bsp_lapic})", cpu.id);

            // Optionally pass data via `extra` if needed later.
            // cpu.extra.store(0, core::sync::atomic::Ordering::Relaxed);

            // Set the AP entry. As soon as we write this, Limine will jump the AP to it.
            cpu.goto_address.write(ap_start);
        }
    } else {
        println!("[smp] Limine MP response not present; running uniprocessor");
    }
}

/// Limine AP entrypoint. Signature is mandated by limine::mp::GotoAddress::write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ap_start(cpu: &MpCpu) -> ! {
    // Per-CPU data and core-local tables
    switch_to_kernel_page();
    tlb_flush_all_including_global();
    unsafe { init_this_cpu_percpu() };
    gdt::init_current_cpu();
    interrupts::init_current_cpu();

    // Enable LAPIC
    unsafe { enable_lapic() };

    unsafe { setup_syscall() };

    unsafe { fpu::init_fpu() };

    thread::scheduler::init();

    println!(
        "[smp] AP online: LAPIC id {} {}, {}",
        crate::acpi::raw_current_apic_id(),
        cpu.lapic_id,
        get_lapic().id()
    );

    queue_spawn_kthread_named("test", kthread_test as u64);

    set_apic_timer_and_enable(Duration::from_millis(5));

    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

pub fn kthread_test() -> ! {
    let logger = sched().get_logger();
    loop {
        log!(logger, "hello multiple cpus");
        sched().thread_wait_timeout(Duration::from_millis(500));
    }
}

#[inline(always)]
pub fn tlb_flush_all_including_global() {
    unsafe {
        use core::arch::asm;
        const CR4_PGE: u64 = 1 << 7;

        // Save CR4
        let mut cr4_orig: u64;
        asm!("mov {}, cr4", out(reg) cr4_orig, options(nomem, preserves_flags));

        // Clear PGE
        let cr4_no_pge = cr4_orig & !CR4_PGE;
        asm!("mov cr4, {}", in(reg) cr4_no_pge, options(nomem, preserves_flags));

        // Reload CR3 (flushes all non-globals; with PGE off, globals are dropped too)
        let mut cr3: u64;
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, preserves_flags));
        asm!("mov cr3, {}", in(reg) cr3, options(nomem, preserves_flags));

        // Restore CR4 (re-enable PGE as before)
        asm!("mov cr4, {}", in(reg) cr4_orig, options(nomem, preserves_flags));
    }
}
