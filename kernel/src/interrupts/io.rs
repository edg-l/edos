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
        waitqueue::WaitQueue,
    },
};

pub static XHCI_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();
pub static E1000E_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();
pub static HDA_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();
pub static NVME_DRIVER_THREAD_ID: Once<Weak<Thread>> = Once::new();

/// Total NVMe MSI-X/MSI/INTx interrupts received. The admin and I/O
/// completion queues share this one vector (Create I/O CQ CDW11.IV = 0), so
/// the handler cannot tell which queue posted a completion; the dispatcher
/// drains both on every wake.
pub static NVME_IRQS_FIRED: AtomicU64 = AtomicU64::new(0);

/// Total AHCI MSIs received (incremented by the interrupt handler).
/// Used alongside the dispatcher's wake count to diagnose NCQ-timeout
/// root cause: if IRQs keep firing but timeout still happens, the wake
/// path is fine and the drive genuinely stalled.
pub static AHCI_IRQS_FIRED: AtomicU64 = AtomicU64::new(0);

/// Completions the virtio-gpu control queue has signalled.
///
/// The handler cannot drain the ring itself: the queue lives behind the
/// `DISPLAY` lock, which a flip holds with preemption disabled, so an interrupt
/// arriving on that CPU would deadlock against it. It publishes a count
/// instead, and the flip path reads it to know a poll is worth making. Counting
/// rather than flagging means a completion that lands between two flips is not
/// lost.
pub static VIRTIO_GPU_IRQS_FIRED: AtomicU64 = AtomicU64::new(0);

/// Where a thread waiting for the display to finish reading a frame parks.
///
/// The handler cannot touch the queue itself -- that is behind `DISPLAY`, which
/// a flip holds with preemption disabled -- so it publishes the count above and
/// wakes whoever is here. The waiter re-takes `DISPLAY` and looks.
pub static VIRTIO_GPU_WAITERS: WaitQueue = WaitQueue::new();

pub(super) extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    crate::drivers::ps2_drain_buffer();
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn virtio_gpu_interrupt_handler(
    _stack_frame: InterruptStackFrame,
) {
    VIRTIO_GPU_IRQS_FIRED.fetch_add(1, Ordering::Relaxed);
    VIRTIO_GPU_WAITERS.wake_all_irq();
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn ahci_interrupt_handler(_stack_frame: InterruptStackFrame) {
    AHCI_IRQS_FIRED.fetch_add(1, Ordering::Relaxed);
    if let Some(handle) = AHCI_DRIVER_THREAD_ID.get() {
        sched().wake_thread_irq(handle, WakePriority::Interrupt);
    }
    unsafe { get_lapic().end_of_interrupt() };
}

pub(super) extern "x86-interrupt" fn nvme_interrupt_handler(_stack_frame: InterruptStackFrame) {
    NVME_IRQS_FIRED.fetch_add(1, Ordering::Relaxed);
    if let Some(handle) = NVME_DRIVER_THREAD_ID.get() {
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
