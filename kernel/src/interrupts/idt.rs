use core::sync::atomic::Ordering;

use x86_64::{
    PrivilegeLevel, VirtAddr,
    registers::control::Cr2,
    structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode},
};

use crate::{
    apic::get_lapic,
    drivers::keyboard::keyboard_interrupt_handler,
    gdt,
    interrupts::{
        InterruptIndex,
        io::{ahci_interrupt_handler, device_not_available_handler, mouse_interrupt_handler},
    },
    log, println,
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
        idt.page_fault
            .set_handler_fn(page_fault_handler)
            .set_stack_index(gdt::PAGE_FAULT_IST_INDEX);
    }

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
    panic!(
        "EXCEPTION: DOUBLE FAULT: ({error_code})\n{:#?}",
        stack_frame
    );
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
        log!("Page fault");
        log!("Accessed Address: {address:?}");
        log!("Error Code: {error_code:?}");
        log!("Error Desc: {error_desc:?}");
        log!("RIP: {:p}", stack_frame.instruction_pointer);
        log!("Stack: {:p}", stack_frame.stack_pointer);
        log!("Fault Type: {error_desc}");
        sched().thread_exit(11);
    }
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
