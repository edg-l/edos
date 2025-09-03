use acpi::HpetInfo;
use spin::Once;
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{PageTableFlags, Translate, mapper::TranslateResult},
};

use crate::{
    acpi::acpi_tables,
    drivers::hpet::instant::HpetTimer,
    memory::{get_virt_addr, mapper::memory_mapper},
    println,
};

const HPET_GENERAL_CAPS: usize = 0x00;
const HPET_GENERAL_CONFIG: usize = 0x10;
const HPET_MAIN_COUNTER: usize = 0xF0;

static HPET: Once<HpetTimer> = Once::new();

pub fn init() {
    let tables = acpi_tables();

    let result = HpetInfo::new(tables);

    match result {
        Ok(hpet_info) => {
            println!("HPET base addr at 0x{:x}", hpet_info.base_address);
            let hpet_base = PhysAddr::new(hpet_info.base_address as u64);
            // HPET is mapped by limine.
            let virt_hpet = get_virt_addr(hpet_base);

            let mut mapper = memory_mapper();

            match mapper.mapper.translate(virt_hpet) {
                TranslateResult::Mapped {
                    frame,
                    offset,
                    flags,
                } => {
                    println!("already mapped: {frame:?} {offset} {flags:?}");
                }
                TranslateResult::NotMapped => {
                    println!("HPET not mapped, mapping");
                    if mapper
                        .map_address(
                            virt_hpet,
                            hpet_base,
                            PageTableFlags::PRESENT
                                | PageTableFlags::WRITABLE
                                | PageTableFlags::NO_CACHE,
                        )
                        .is_err()
                    {
                        println!("failed to map hpet, already mapped");
                    }
                }
                TranslateResult::InvalidFrameAddress(_) => {
                    unreachable!()
                }
            }
            println!("HPET virt base addr at 0x{:x}", virt_hpet);

            HPET.call_once(|| {
                let mut timer = HpetTimer {
                    frequency: 0,
                    base: virt_hpet,
                };

                let caps = timer.get_caps();

                // Frequency in femtoseconds per tick (bits 32-63)
                let frequency = caps >> 32;
                timer.frequency = frequency;
                unsafe { timer.enable() };

                timer
            });
        }
        Err(err) => {
            println!("Couldn't find HPET: {err:#?}");
        }
    }
}

pub fn get_hpet_timer() -> Option<&'static HpetTimer> {
    HPET.get()
}

impl HpetTimer {
    unsafe fn read_hpet_reg(&self, offset: usize) -> u64 {
        unsafe { core::ptr::read_volatile((self.base.as_u64() + offset as u64) as *const u64) }
    }

    unsafe fn write_hpet_reg(&self, offset: usize, value: u64) {
        unsafe {
            core::ptr::write_volatile((self.base.as_u64() + offset as u64) as *mut u64, value)
        };
    }

    unsafe fn enable(&self) {
        unsafe {
            let config = self.read_hpet_reg(HPET_GENERAL_CONFIG);
            self.write_hpet_reg(HPET_GENERAL_CONFIG, config | 1); // enable bit
        }
    }

    pub fn get_caps(&self) -> u64 {
        unsafe { self.read_hpet_reg(HPET_GENERAL_CAPS) }
    }

    pub fn get_counter(&self) -> u64 {
        unsafe { self.read_hpet_reg(HPET_MAIN_COUNTER) }
    }
}
