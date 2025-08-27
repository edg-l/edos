#![no_std]
#![no_main]

use x86_64::instructions::hlt;

use crate::boot::BootInfo;

mod boot;

fn main(info: BootInfo) -> ! {
    for i in 0..100_u64 {
        // Calculate the pixel offset using the framebuffer information we obtained above.
        // We skip `i` scanlines (pitch is provided in bytes) and add `i * 4` to skip `i` pixels forward.
        let pixel_offset = i * info.framebuffer.pitch() + i * 4;

        // Write 0xFFFFFFFF to the provided pixel offset to fill it white.
        unsafe {
            info.framebuffer
                .addr()
                .add(pixel_offset as usize)
                .cast::<u32>()
                .write(0xFFFFFFFF)
        };
    }

    loop {
        hlt();
    }
}

#[panic_handler]
fn rust_panic(_info: &core::panic::PanicInfo) -> ! {
    loop {
        hlt();
    }
}
