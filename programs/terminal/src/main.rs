#![no_std]
#![no_main]

use elibc::{println, sys_getpid};

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    println!("Terminal starting...");
    println!("Process ID: {}", sys_getpid());

    0
}
