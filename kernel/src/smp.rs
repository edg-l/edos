use core::{hint::spin_loop, sync::atomic::AtomicUsize, time::Duration};

use limine::mp::Cpu as MpCpu;

use crate::{
    apic::{get_lapic, init::enable_lapic},
    boot::MP_REQUEST,
    drivers::fpu,
    gdt, interrupts, log, println,
    syscalls::setup_syscall,
    thread::{self, scheduler::switch_to_kernel_page},
    timer::Instant,
    util::per_cpu::init_gs_for_this_cpu,
};

pub static NUM_CPUS: AtomicUsize = AtomicUsize::new(0);

/// Initialize SMP using Limine's MP request: set AP entrypoints and let Limine bring them up.
pub fn init() {
    // Ensure the request is referenced so the linker keeps it.
    if let Some(resp) = MP_REQUEST.get_response() {
        let bsp_lapic = resp.bsp_lapic_id();
        for &cpu in resp.cpus() {
            NUM_CPUS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // Skip the BSP; it is already running `init()` and `main()`.
            if cpu.lapic_id == bsp_lapic {
                continue;
            }

            log!("smp: booting AP {} (BSP={})", cpu.id, bsp_lapic);

            // Optionally pass data via `extra` if needed later.
            // cpu.extra.store(0, core::sync::atomic::Ordering::Relaxed);

            // Set the AP entry. As soon as we write this, Limine will jump the AP to it.
            cpu.goto_address.write(ap_start);
        }
    } else {
        println!("[smp] Limine MP response not present; running uniprocessor");
    }

    let now = Instant::now();
    while now.elapsed() < Duration::from_millis(100) {
        spin_loop();
    }
}

/// Limine AP entrypoint. Signature is mandated by limine::mp::GotoAddress::write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ap_start(cpu: &MpCpu) -> ! {
    // Per-CPU data and core-local tables
    switch_to_kernel_page();
    tlb_flush_all_including_global();
    unsafe { init_gs_for_this_cpu(cpu.lapic_id) };
    gdt::init_current_cpu();
    interrupts::init_current_cpu();

    // Enable LAPIC
    unsafe { enable_lapic() };

    // PAT must come after GDT/IDT/GS init since it uses println
    crate::memory::pat::init_pat();

    unsafe { setup_syscall() };

    unsafe { fpu::init_fpu() };

    thread::scheduler::init();

    println!("[smp] AP online: LAPIC id {}", unsafe { get_lapic().id() });

    loop {
        x86_64::instructions::interrupts::enable_and_hlt();
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
