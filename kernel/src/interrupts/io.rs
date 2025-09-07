use x86_64::structures::idt::InterruptStackFrame;

use crate::{
    apic::get_lapic,
    drivers::ahci::{AHCI_DRIVER_THREAD_ID, mark_interrupt_pending},
    thread::scheduler::sched,
};

pub(super) extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    mark_interrupt_pending();

    if let Some(tid) = AHCI_DRIVER_THREAD_ID.get() {
        sched().thread_wake(tid.clone());
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn device_not_available_handler(
    _stack_frame: InterruptStackFrame,
) {
    panic!("Device not available");
}
