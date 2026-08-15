use crate::interrupts::idt::build_idt_for_current_cpu;
use alloc::boxed::Box;
use x86_64::structures::idt::InterruptDescriptorTable;

pub mod idt;
pub mod io;

pub const APIC_OFFSET: u8 = 32;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = APIC_OFFSET,
    Keyboard = APIC_OFFSET + 1,
    Mouse = APIC_OFFSET + 2,
    Error = APIC_OFFSET + 3,
    Ahci = APIC_OFFSET + 6,
    Reschedule = APIC_OFFSET + 7,
    Xhci = APIC_OFFSET + 8,
    E1000e = APIC_OFFSET + 9,
    Hda = APIC_OFFSET + 10,
    TlbShootdown = APIC_OFFSET + 11,
    VirtioGpu = APIC_OFFSET + 12,
    Spurious = 0xFF,
}

impl InterruptIndex {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Initialize interrupt handling for the current CPU by loading the shared IDT.
/// Must be called once on every CPU after its GDT/TSS is initialized.
pub fn init_current_cpu() {
    let idt = build_idt_for_current_cpu();
    let idt_static: &'static mut InterruptDescriptorTable = Box::leak(Box::new(idt));
    idt_static.load();
}
