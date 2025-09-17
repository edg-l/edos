#![no_std]
#![no_main]

extern crate alloc;

mod terminal;

#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    terminal::run()
}
