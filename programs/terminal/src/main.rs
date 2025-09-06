#![no_std]
#![no_main]

use elibc::{
    graphics::{sys_draw_rect, sys_render, sys_screen_info},
    println, sys_getpid, sys_read, sys_write,
};
use spin::Once;

const RED: u32 = 0xFF0000FF;
const GREEN: u32 = 0x00FF00FF;
const BLUE: u32 = 0x0000FFFF;

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let screen = sys_screen_info().unwrap();

    // Test stdin reading
    println!("Type something and press Enter:");

    let mut buffer = [0u8; 256];
    match unsafe { sys_read(0, buffer.as_mut_ptr(), buffer.len()) } {
        n if n > 0 => {
            println!(
                "You typed: {:?}",
                core::str::from_utf8(&buffer[..n as usize]).unwrap_or("invalid utf8")
            );

            // Echo it back
            unsafe { sys_write(1, buffer.as_ptr(), n as usize) };
        }
        0 => {
            println!("No input received (EOF)");
        }
        -1 => {
            println!("Error reading from stdin");
        }
        _ => {
            println!("Unexpected return value");
        }
    }

    // Test multiple reads
    println!("Type 'quit' to exit, anything else to echo:");
    loop {
        let mut buffer = [0u8; 256];
        match unsafe { sys_read(0, buffer.as_mut_ptr(), buffer.len()) } {
            n if n > 0 => {
                let input = core::str::from_utf8(&buffer[..n as usize]).unwrap_or("invalid");
                let trimmed = input.trim();

                if trimmed == "quit" {
                    println!("Goodbye!");
                    break;
                }

                println!("Echo: {}", trimmed);
            }
            0 => {
                println!("EOF received");
                break;
            }
            -1 => {
                println!("Error reading stdin");
                break;
            }
            _ => {
                println!("Unexpected return value");
                break;
            }
        }
    }

    0
}
