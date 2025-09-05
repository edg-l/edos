#![no_std]
#![no_main]

use elibc::{println, sys_getpid};
use spin::Once;

static TEST: Once<u64> = Once::new();

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    println!("Terminal starting...");
    println!("Process ID: {}", sys_getpid());

    let x = TEST.call_once(|| 2);

    println!("Called once {x}");

    0
}
