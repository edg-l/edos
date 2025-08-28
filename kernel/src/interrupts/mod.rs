use crate::interrupts::idt::IDT;

pub mod idt;

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

pub fn init() {
    IDT.load();
}
