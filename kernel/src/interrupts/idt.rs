use spin::Lazy;
use x86_64::{
    PrivilegeLevel,
    structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode},
};

#[allow(unused)]
pub const PS2_DATA_PORT: u16 = 0x60;
#[allow(unused)]
pub const PS2_COMMAND_PORT: u16 = 0x64;

use crate::{apic::LAPIC, gdt, interrupts::InterruptIndex, serial_println};

pub static IDT: spin::Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    }
    idt.alignment_check.set_handler_fn(alignment_check_handler);
    idt.general_protection_fault
        .set_handler_fn(general_protection_fault_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    idt.device_not_available
        .set_handler_fn(device_not_available_handler);
    idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
    idt[InterruptIndex::Error.as_u8()].set_handler_fn(apic_error_interrupt_handler);
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
    idt[InterruptIndex::Mouse.as_u8()].set_handler_fn(mouse_interrupt_handler);
    idt[InterruptIndex::Ahci.as_u8()].set_handler_fn(ahci_interrupt_handler);
    idt[InterruptIndex::Spurious.as_u8()].set_handler_fn(spurious_interrupt_handler);
    idt
});

extern "x86-interrupt" fn general_protection_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: u64,
) {
    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring0 {
        panic!(
            "EXCEPTION: general_protection_fault (segfault?) CHECK: (error: {error_code})\n{stack_frame:#?}"
        );
    } else {
        serial_println!(
            "EXCEPTION: general_protection_fault (segfault?) in RING 3 CHECK: (error: {error_code})\n{stack_frame:#?}",
        );
        serial_println!(
            "EXCEPTION: general_protection_fault (segfault?) in RING 3: (error: {error_code})\n{stack_frame:#?}",
        );

        unsafe { LAPIC.write().end_of_interrupt() };
    }
}

extern "x86-interrupt" fn invalid_opcode_handler(stack_frame: InterruptStackFrame) {
    serial_println!("EXCEPTION: invalid_opcode CHECK\n{stack_frame:#?}");
    unsafe { LAPIC.write().end_of_interrupt() };
}

extern "x86-interrupt" fn alignment_check_handler(stack_frame: InterruptStackFrame, value: u64) {
    serial_println!("EXCEPTION: ALIGNMENT CHECK: ({value})\n{stack_frame:#?}");
    unsafe { LAPIC.write().end_of_interrupt() };
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

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    //serial_println!("timer interrupt");
    unsafe { LAPIC.write().end_of_interrupt() };
}

// Timer interrupt handler for preemptive multitasking
// We need naked assembly to save/restore all registers properly
#[unsafe(naked)]
extern "x86-interrupt" fn _timer_interrupt_handler_cswitch(_stack_frame: InterruptStackFrame) {
    core::arch::naked_asm!(
        // Save all registers first
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",

        // check CS
        "test qword ptr [rsp + 128], 3",  // Test CS directly from stack
        "jz 2f",                 // Jump if from kernel mode

        // Coming from user mode - need swapgs
        "swapgs",

        "2:",
        // Call context switch function
        "mov rdi, rsp",
        "call {context_switch}",

         // Check CS
        "test qword ptr [rsp + 128], 3",  // Test CS
        "jz 3f",
        "swapgs",                // Returning to user mode

        "3:",

        // Restore all registers (including correct RAX!)
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",

        "iretq",

        context_switch = sym timer_context_switch_handler,
    )
}

// Called from naked assembly with pointer to saved registers
#[expect(unused)]
extern "C" fn timer_context_switch_handler(_saved_registers: *mut u64) {
    // Acknowledge interrupt first to prevent stacking
    unsafe { LAPIC.write().end_of_interrupt() };
}

extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { LAPIC.write().end_of_interrupt() };
}

extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { LAPIC.write().end_of_interrupt() };
}

extern "x86-interrupt" fn apic_error_interrupt_handler(_stack_frame: InterruptStackFrame) {
    serial_println!("apic error");
    unsafe { LAPIC.write().end_of_interrupt() };
}

extern "x86-interrupt" fn spurious_interrupt_handler(_stack_frame: InterruptStackFrame) {
    serial_println!("spurious");
    unsafe { LAPIC.write().end_of_interrupt() };
}

extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    use x86_64::registers::control::Cr2;

    let error_desc = decode_page_fault_error(error_code);

    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring0 {
        let address = Cr2::read().unwrap();
        serial_println!("EXCEPTION: PAGE FAULT in Ring3");
        serial_println!("Accessed Address: {address:?}");
        serial_println!("Error Code: {error_code:?}");
        serial_println!("{stack_frame:#?}");
        serial_println!("Fault Type: {error_desc}",);

        serial_println!(
            "Page fault, address = {:p}, error = {error_desc:?}",
            address.as_ptr::<u8>()
        );
        panic!("EXCEPTION: PAGE FAULT IN RING 0");
    } else {
        let address = Cr2::read().unwrap();
        serial_println!("EXCEPTION: PAGE FAULT in Ring3");
        serial_println!("Accessed Address: {address:?}");
        serial_println!("Error Code: {error_code:?}");
        serial_println!("{stack_frame:#?}");
        serial_println!("Fault Type: {error_desc}");

        serial_println!(
            "Page fault, address = {:p}, error = {error_desc:?}",
            address.as_ptr::<u8>()
        );
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

extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {}

extern "x86-interrupt" fn device_not_available_handler(_stack_frame: InterruptStackFrame) {}
