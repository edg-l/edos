#![no_std]
#![no_main]

extern crate alloc;

mod terminal;

#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: isize, _argv: *const *const u8) -> i32 {
    terminal::run()
}
