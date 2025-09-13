#![no_std]
#![no_main]

use core::hint::spin_loop;

use elibc::println;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    println!("hello world from program");

    loop {
        spin_loop();
    }
    0
}
