use x86_64::structures::idt::InterruptStackFrame;

use crate::apic::{get_lapic};

pub(super) extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn device_not_available_handler(
    _stack_frame: InterruptStackFrame,
) {
    panic!("Device  not available");
}
