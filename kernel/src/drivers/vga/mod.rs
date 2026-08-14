//! Bochs/QEMU VBE extensions (the DISPI interface), used by the framebuffer to
//! size video memory and to page-flip by moving the visible Y offset.

use x86_64::instructions::port::Port;

const DISPI_INDEX_PORT: u16 = 0x01CE;
const DISPI_DATA_PORT: u16 = 0x01CF;

pub const DISPI_INDEX_VIRT_HEIGHT: u16 = 0x07;
pub const DISPI_INDEX_Y_OFFSET: u16 = 0x09;
pub const DISPI_INDEX_VIDEO_MEMORY_64K: u16 = 0x0A;

pub fn dispi_write(index: u16, value: u16) {
    unsafe {
        Port::new(DISPI_INDEX_PORT).write(index);
        Port::new(DISPI_DATA_PORT).write(value);
    }
}

pub fn dispi_read(index: u16) -> u16 {
    unsafe {
        Port::new(DISPI_INDEX_PORT).write(index);
        Port::new(DISPI_DATA_PORT).read()
    }
}
