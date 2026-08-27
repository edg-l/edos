use acpi::HpetInfo;
use spin::Once;
use x86_64::{
    PhysAddr,
    structures::paging::{PageTableFlags, Translate, mapper::TranslateResult},
};

use crate::{
    acpi::acpi_tables,
    drivers::hpet::instant::HpetTimer,
    memory::{get_virt_addr_from_phys_offset, mapper::memory_mapper, valloc::vmalloc},
    println,
};

const HPET_GENERAL_CAPS: usize = 0x00;
const HPET_GENERAL_CONFIG: usize = 0x10;
const HPET_MAIN_COUNTER: usize = 0xF0;

static HPET: Once<HpetTimer> = Once::new();

pub fn init() {
    if HPET.is_completed() {
        return;
    }

    let tables = acpi_tables();

    let result = HpetInfo::new(tables);

    match result {
        Ok(hpet_info) => {
            println!("HPET base addr at 0x{:x}", hpet_info.base_address);
            let hpet_base = PhysAddr::new(hpet_info.base_address as u64);
            // HPET is sometimes mapped by limine.
            let mut virt_hpet = get_virt_addr_from_phys_offset(hpet_base);

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
                    virt_hpet = vmalloc(4096).expect("vmalloc: no address space for HPET MMIO");
                    if mapper
                        .map_address(
                            virt_hpet,
                            hpet_base,
                            PageTableFlags::PRESENT
                                | PageTableFlags::WRITABLE
                                | PageTableFlags::NO_CACHE
                                | PageTableFlags::GLOBAL,
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
                // SAFETY: `virt_hpet` was mapped over the base address the
                // ACPI HPET table reported, a few lines above, so `timer`'s
                // register window is live.
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
    /// # Safety
    /// `self.base` must be a mapped HPET register window, and `offset` must
    /// name a register inside it. Every HPET register is a naturally aligned
    /// 64-bit location, so `offset` must be a multiple of 8.
    unsafe fn read_hpet_reg(&self, offset: usize) -> u64 {
        // SAFETY: the caller guarantees `self.base + offset` is a naturally
        // aligned register inside the mapped window. Volatile because the
        // counter and status registers change under the driver.
        unsafe { core::ptr::read_volatile((self.base.as_u64() + offset as u64) as *const u64) }
    }

    /// # Safety
    /// As [`Self::read_hpet_reg`], and `offset` must name a writable register:
    /// the main counter and the configuration register are, the capability
    /// register is not.
    unsafe fn write_hpet_reg(&self, offset: usize, value: u64) {
        // SAFETY: the caller guarantees `self.base + offset` is a naturally
        // aligned writable register inside the mapped window.
        unsafe {
            core::ptr::write_volatile((self.base.as_u64() + offset as u64) as *mut u64, value)
        };
    }

    /// Set `ENABLE_CNF`, which starts the main counter.
    ///
    /// # Safety
    /// `self.base` must be a mapped HPET register window.
    unsafe fn enable(&self) {
        // SAFETY: `HPET_GENERAL_CONFIG` is an 8-aligned writable register of
        // the window this function's contract requires; the read-modify-write
        // only sets bit 0 and leaves every other configuration bit alone.
        unsafe {
            let config = self.read_hpet_reg(HPET_GENERAL_CONFIG);
            self.write_hpet_reg(HPET_GENERAL_CONFIG, config | 1); // enable bit
        }
    }

    pub fn get_caps(&self) -> u64 {
        // SAFETY: an `HpetTimer` only exists once `init` mapped its window, and
        // `HPET_GENERAL_CAPS` is an 8-aligned register inside it.
        unsafe { self.read_hpet_reg(HPET_GENERAL_CAPS) }
    }

    pub fn get_counter(&self) -> u64 {
        // SAFETY: as `get_caps` -- the window is mapped for the life of the
        // timer and `HPET_MAIN_COUNTER` is an 8-aligned register inside it.
        unsafe { self.read_hpet_reg(HPET_MAIN_COUNTER) }
    }
}
