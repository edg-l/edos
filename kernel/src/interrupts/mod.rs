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
