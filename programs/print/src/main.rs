#![no_std]
#![no_main]


use elibc::println;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    println!("hello world from program");

    0
}
