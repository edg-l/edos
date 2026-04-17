use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Weak;
use spin::Once;
use x86_64::structures::idt::InterruptStackFrame;

use crate::{
    apic::get_lapic,
    drivers::ahci::AHCI_DRIVER_THREAD_ID,
    thread::{
        scheduler::{WakePriority, sched},
        thread::Thread,
    },
};

pub static XHCI_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();
pub static E1000E_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();
pub static HDA_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();

/// Total AHCI MSIs received (incremented by the interrupt handler).
/// Used alongside the dispatcher's wake count to diagnose NCQ-timeout
/// root cause: if IRQs keep firing but timeout still happens, the wake
/// path is fine and the drive genuinely stalled.
pub static AHCI_IRQS_FIRED: AtomicU64 = AtomicU64::new(0);

pub(super) extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::drivers::ps2_drain_buffer();
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    AHCI_IRQS_FIRED.fetch_add(1, Ordering::Relaxed);
    if let Some(handle) = AHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(handle, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn xhci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if let Some(handle) = XHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(handle, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn e1000e_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if let Some(handle) = E1000E_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(handle, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn hda_interrupt_handler(_stack_frame: InterruptStackFrame) {
    if let Some(handle) = HDA_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(handle, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn device_not_available_handler(
    _stack_frame: InterruptStackFrame,
) {
    panic!("Device not available");
}
