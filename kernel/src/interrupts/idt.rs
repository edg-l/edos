use core::sync::atomic::Ordering;

use x86_64::{
    PrivilegeLevel, VirtAddr,
    registers::control::Cr2,
    structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode},
    structures::paging::PageTableFlags,
};

use crate::{
    apic::get_lapic,
    boot::boot_info,
    drivers::keyboard::keyboard_interrupt_handler,
    gdt,
    interrupts::{
        InterruptIndex,
        io::{
            ahci_interrupt_handler, device_not_available_handler, e1000e_interrupt_handler,
            hda_interrupt_handler, mouse_interrupt_handler, xhci_interrupt_handler,
        },
    },
    log,
    memory::cow::handle_cow_fault,
    println,
    thread::{interrupt::timer_interrupt_handler, scheduler::sched},
    util::uaccess::current_cpu_uaccess,
};

/// Build an IDT instance for the current CPU.
///
/// The descriptor layout is identical across CPUs, but each CPU will load its
/// own copy to allow future per-CPU customization if desired.
pub fn build_idt_for_current_cpu() -> InterruptDescriptorTable {
    let mut idt = InterruptDescriptorTable::new();

    // Avoid assigning the timer/device interrupts to the shared IST stack while the
    // scheduler runs with interrupts enabled: re-entrancy would reset the stack pointer
    // to the IST top and corrupt the in-flight context frame.
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    }
    // Page faults use the current kernel stack (no IST). This allows nested
    // page faults (e.g., vmalloc fault during demand fault handling) to work
    // naturally by pushing a new frame on the same stack. Kernel stack
    // overflow hits the guard page and escalates to a double fault, which IS
    // on its own IST and will be caught.
    idt.page_fault.set_handler_fn(page_fault_handler);

    idt.alignment_check.set_handler_fn(alignment_check_handler);
    idt.general_protection_fault
        .set_handler_fn(general_protection_fault_handler);

    // Note: dont use shared ist stack
    unsafe {
        idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);

        idt.device_not_available
            .set_handler_fn(device_not_available_handler);
        idt[InterruptIndex::Timer.as_u8()]
            .set_handler_addr(VirtAddr::new(timer_interrupt_handler as *mut u8 as u64));

        idt[InterruptIndex::Error.as_u8()].set_handler_fn(apic_error_interrupt_handler);
        idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
        idt[InterruptIndex::Mouse.as_u8()].set_handler_fn(mouse_interrupt_handler);
        idt[InterruptIndex::Ahci.as_u8()].set_handler_fn(ahci_interrupt_handler);
        idt[InterruptIndex::Xhci.as_u8()].set_handler_fn(xhci_interrupt_handler);
        idt[InterruptIndex::E1000e.as_u8()].set_handler_fn(e1000e_interrupt_handler);
        idt[InterruptIndex::Hda.as_u8()].set_handler_fn(hda_interrupt_handler);
        idt[InterruptIndex::TlbShootdown.as_u8()]
            .set_handler_fn(crate::memory::tlb::tlb_shootdown_handler);
        idt[InterruptIndex::Spurious.as_u8()].set_handler_fn(spurious_interrupt_handler);
        idt[InterruptIndex::Reschedule.as_u8()]
            .set_handler_addr(VirtAddr::new(timer_interrupt_handler as *mut u8 as u64));
    }

    idt
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    unsafe { get_lapic().end_of_interrupt() };
    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring0 {
        println!("GPF Error code: 0x{:x}", error_code);
        println!("Selector index: {}", (error_code >> 3) & 0x1FFF);
        println!("Table: {}", if error_code & 4 != 0 { "LDT" } else { "GDT" });
        println!("External: {}", error_code & 1 != 0);
        println!("Stack frame: {:#?}", stack_frame);

        panic!("General Protection Fault");
    } else {
        log!("GPF Error code: 0x{:x}", error_code);
        log!("GPF Error code: 0x{:x}", error_code);
        log!("Selector index: {}", (error_code >> 3) & 0x1FFF);
        log!("Table: {}", if error_code & 4 != 0 { "LDT" } else { "GDT" });
        log!("External: {}", error_code & 1 != 0);
        log!("Stack frame: {:#?}", stack_frame);

        sched().thread_exit(135);
    }
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    unsafe { get_lapic().end_of_interrupt() };

    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
        log!("Invalid opcode, forcing exit, {stack_frame:#?}");
        sched().thread_exit(135);
    } else {
        println!("EXCEPTION: invalid_opcode CHECK\n{stack_frame:#?}");
        panic!();
    }
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn alignment_check_handler(stack_frame: InterruptStackFrame, value: u64) {
    unsafe { get_lapic().end_of_interrupt() };

    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
        log!("EXCEPTION: ALIGNMENT CHECK: ({value})\n{stack_frame:#?}");
        sched().thread_exit(135);
    } else {
        println!("EXCEPTION: ALIGNMENT CHECK: ({value})\n{stack_frame:#?}");
        panic!();
    }
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) -> ! {
    // Do NOT use panic!/println! here -- the serial lock or allocator lock
    // may be held by the faulting code, which would deadlock and triple-fault.
    crate::serial::emergency_write(b"\n!!! DOUBLE FAULT (error=");
    // Write error code as decimal digits
    let mut buf = [0u8; 20];
    let s = crate::util::fmt_u64(error_code, &mut buf);
    crate::serial::emergency_write(s);
    crate::serial::emergency_write(b")\nRIP=0x");
    let rip = stack_frame.instruction_pointer.as_u64();
    let s = crate::util::fmt_hex(rip, &mut buf);
    crate::serial::emergency_write(s);
    crate::serial::emergency_write(b"\nRSP=0x");
    let rsp = stack_frame.stack_pointer.as_u64();
    let s = crate::util::fmt_hex(rsp, &mut buf);
    crate::serial::emergency_write(s);
    crate::serial::emergency_write(b"\nCS=0x");
    let cs = stack_frame.code_segment.0 as u64;
    let s = crate::util::fmt_hex(cs, &mut buf);
    crate::serial::emergency_write(s);
    crate::serial::emergency_write(b"\n");

    loop {
        x86_64::instructions::hlt();
    }
}

extern "x86-interrupt" fn apic_error_interrupt_handler(_stack_frame: InterruptStackFrame) {
    println!("apic error");
    unsafe { get_lapic().end_of_interrupt() };
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn spurious_interrupt_handler(_stack_frame: InterruptStackFrame) {
    // apic doesnt require EOI for spurious
}

#[unsafe(no_mangle)]
extern "x86-interrupt" fn page_fault_handler(
    mut stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // Note: do not add complex calls or memory read or scheduler reads, otherwise recursive faults can happen.

    let error_desc = decode_page_fault_error(error_code);
    let address = Cr2::read().unwrap();

    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring0 {
        // Demand-fault lazy user pages accessed from kernel (e.g., copy_to_user via HHDM).
        // Must be checked BEFORE uaccess so the page gets mapped and the access succeeds.
        if !error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
            && address.as_u64() < 0x0000_8000_0000_0000
        {
            if unsafe { crate::memory::fault::handle_demand_fault(address, error_code) } {
                return;
            }
        }

        // Check if we're in a user access operation
        let uaccess = current_cpu_uaccess();
        if uaccess.is_active() {
            // Resume at the fault handler
            let resume_addr = uaccess.fault_resume.load(Ordering::Relaxed);
            uaccess.clear();

            // Modify the stack frame to resume at the fault handler
            unsafe {
                stack_frame.as_mut().update(|frame| {
                    frame.instruction_pointer = VirtAddr::new(resume_addr);
                });
            }

            // Return from interrupt - execution will resume at fault handler
            return;
        }

        // Lazy vmalloc fault fixup: if the faulting address is in the kernel
        // dynamic memory range and the kernel page table has a valid mapping,
        // this is a stale paging-structure cache entry on another CPU. Flush
        // the TLB entry and retry.
        if vmalloc_fault(address) {
            return;
        }

        println!("EXCEPTION: PAGE FAULT in Ring 0");
        log!("Accessed Address: {address:?}");
        log!("Error Code: {error_code:?}");
        log!("Error Desc: {error_desc:?}");
        log!("RIP: {:p}", stack_frame.instruction_pointer);
        log!("Stack: {:p}", stack_frame.stack_pointer);
        log!("Fault Type: {error_desc}");

        println!("{stack_frame:#?}");

        panic!("EXCEPTION: PAGE FAULT IN RING 0");
    } else {
        let is_cow_candidate = error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
            && error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE)
            && error_code.contains(PageFaultErrorCode::USER_MODE);

        if is_cow_candidate {
            if unsafe { handle_cow_fault(address) } {
                return;
            }
        }

        // Demand paging: handle non-present pages backed by VMAs
        if !error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION) {
            if unsafe { crate::memory::fault::handle_demand_fault(address, error_code) } {
                return;
            }
        }

        // Use println! (direct serial, no heap allocation) so the message is
        // guaranteed to reach serial output from the IST page fault handler.
        println!(
            "KILL: PF addr={:p} rip={:p} err={error_code:?} ({error_desc})",
            address, stack_frame.instruction_pointer
        );

        // Diagnostic: if the PTE is present, dump its flags + phys so we can
        // identify stale/leaked PTEs. Walks CR3 directly through HHDM.
        {
            use x86_64::registers::control::Cr3;
            use x86_64::structures::paging::{PageTable, PhysFrame};
            let (cr3_frame, _) = Cr3::read();
            let phys_off = crate::boot::boot_info().physical_memory_offset;
            let a = address.as_u64();
            let i4 = ((a >> 39) & 0x1FF) as usize;
            let i3 = ((a >> 30) & 0x1FF) as usize;
            let i2 = ((a >> 21) & 0x1FF) as usize;
            let i1 = ((a >> 12) & 0x1FF) as usize;
            let pt = |f: PhysFrame| -> &'static PageTable {
                unsafe { &*(phys_off + f.start_address().as_u64()).as_ptr::<PageTable>() }
            };
            let p4 = pt(cr3_frame);
            if p4[i4].flags().contains(PageTableFlags::PRESENT) {
                let p3 = pt(PhysFrame::containing_address(p4[i4].addr()));
                if p3[i3].flags().contains(PageTableFlags::PRESENT) {
                    let p2 = pt(PhysFrame::containing_address(p3[i3].addr()));
                    if p2[i2].flags().contains(PageTableFlags::PRESENT) {
                        let p1 = pt(PhysFrame::containing_address(p2[i2].addr()));
                        let e = &p1[i1];
                        println!(
                            "PF-debug: pte.flags={:?} pte.phys={:#x}",
                            e.flags(),
                            e.addr().as_u64(),
                        );
                    } else {
                        println!("PF-debug: no PML2 entry for {a:#x}");
                    }
                } else {
                    println!("PF-debug: no PML3 entry for {a:#x}");
                }
            } else {
                println!("PF-debug: no PML4 entry for {a:#x}");
            }
        }

        sched().thread_exit(11);
    }
}

/// Handle page faults in the kernel dynamic memory range (heap expansion,
/// vmalloc). When one CPU maps new pages in the kernel page table, other CPUs
/// may still have stale paging-structure cache entries ("not present" for a PDE
/// or PDPTE that is now present). Instead of sending TLB shootdown IPIs for
/// every new mapping, we lazily fix it up here: walk the kernel page table to
/// verify the mapping exists, flush the local TLB entry, and let the faulting
/// instruction retry.
///
/// This is lock-free: we read the kernel PML4 physical frame from boot info
/// (immutable after boot) and walk the page table via the physical memory
/// offset. Page table entries are 8-byte aligned and written atomically on
/// x86-64, so reading them without locks is safe.
fn vmalloc_fault(address: VirtAddr) -> bool {
    use crate::memory::{KERNEL_HEAP, valloc::VMALLOC_END};

    let addr = address.as_u64();

    // Only handle faults in the kernel dynamic memory range.
    if addr < KERNEL_HEAP.as_u64() || addr >= VMALLOC_END {
        return false;
    }

    let bi = boot_info();
    let phys_offset = bi.physical_memory_offset.as_u64();
    let kernel_pml4_phys = bi.cr3.0.start_address().as_u64();

    // Walk the kernel page table (read-only, no locks needed).
    // PML4
    let p4_idx = (addr >> 39) & 0x1FF;
    let p4_entry = unsafe { read_pte(kernel_pml4_phys + p4_idx * 8, phys_offset) };
    if p4_entry & PageTableFlags::PRESENT.bits() == 0 {
        return false;
    }

    // PDPT
    let p3_phys = p4_entry & 0x000F_FFFF_FFFF_F000;
    let p3_idx = (addr >> 30) & 0x1FF;
    let p3_entry = unsafe { read_pte(p3_phys + p3_idx * 8, phys_offset) };
    if p3_entry & PageTableFlags::PRESENT.bits() == 0 {
        return false;
    }
    if p3_entry & PageTableFlags::HUGE_PAGE.bits() != 0 {
        // 1 GiB huge page, mapped.
        flush_and_return(address);
        return true;
    }

    // PD
    let p2_phys = p3_entry & 0x000F_FFFF_FFFF_F000;
    let p2_idx = (addr >> 21) & 0x1FF;
    let p2_entry = unsafe { read_pte(p2_phys + p2_idx * 8, phys_offset) };
    if p2_entry & PageTableFlags::PRESENT.bits() == 0 {
        return false;
    }
    if p2_entry & PageTableFlags::HUGE_PAGE.bits() != 0 {
        // 2 MiB huge page, mapped.
        flush_and_return(address);
        return true;
    }

    // PT
    let p1_phys = p2_entry & 0x000F_FFFF_FFFF_F000;
    let p1_idx = (addr >> 12) & 0x1FF;
    let p1_entry = unsafe { read_pte(p1_phys + p1_idx * 8, phys_offset) };
    if p1_entry & PageTableFlags::PRESENT.bits() == 0 {
        return false;
    }

    // The mapping exists in the kernel page table but this CPU faulted due
    // to a stale paging-structure cache. Flush and retry.
    flush_and_return(address);
    true
}

unsafe fn read_pte(phys_addr: u64, phys_offset: u64) -> u64 {
    let virt = (phys_offset + phys_addr) as *const u64;
    unsafe { core::ptr::read_volatile(virt) }
}

fn flush_and_return(address: VirtAddr) {
    use x86_64::instructions::tlb;
    use x86_64::structures::paging::{Page, Size4KiB};
    tlb::flush(Page::<Size4KiB>::containing_address(address).start_address());
}

fn decode_page_fault_error(error_code: PageFaultErrorCode) -> &'static str {
    match (
        error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION),
        error_code.contains(PageFaultErrorCode::CAUSED_BY_WRITE),
        error_code.contains(PageFaultErrorCode::USER_MODE),
    ) {
        (false, false, true) => "User read from unmapped page",
        (false, true, true) => "User write to unmapped page",
        (true, false, true) => "User read from protected page",
        (true, true, true) => "User write to protected page",
        (false, false, false) => "Kernel read from unmapped page",
        (false, true, false) => "Kernel write to unmapped page",
        (true, false, false) => "Kernel read from protected page",
        (true, true, false) => "Kernel write to protected page",
    }
}
