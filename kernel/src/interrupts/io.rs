use spin::Once;
use x86_64::structures::idt::InterruptStackFrame;

use crate::{
    apic::get_lapic,
    drivers::ahci::AHCI_DRIVER_THREAD_ID,
    thread::{
        scheduler::{WakePriority, sched},
        thread::ThreadId,
    },
};

pub static XHCI_DRIVER_THREAD_ID: Once<ThreadId> = Once::new();
pub static E1000E_DRIVER_THREAD_ID: Once<ThreadId> = Once::new();

pub(super) extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::drivers::ps2_drain_buffer();
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if let Some(tid) = AHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(*tid, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn xhci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if let Some(tid) = XHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(*tid, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn e1000e_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if let Some(tid) = E1000E_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(*tid, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn device_not_available_handler(
    _stack_frame: InterruptStackFrame,
) {
    panic!("Device not available");
}
