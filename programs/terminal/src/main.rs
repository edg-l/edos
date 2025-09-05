#![no_std]
#![no_main]

use elibc::{
    graphics::{sys_draw_rect, sys_render, sys_screen_info},
    println, sys_getpid,
};
use spin::Once;

static TEST: Once<u64> = Once::new();

const RED: u32 = 0xFF0000FF;
const GREEN: u32 = 0x00FF00FF;
const BLUE: u32 = 0x0000FFFF;

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    println!("Terminal starting...");
    println!("Process ID: {}", sys_getpid());

    match sys_screen_info() {
        Ok(screen) => {
            println!("Screen: {}x{}", screen.width, screen.height);

            let rect_width = 200;
            let rect_height = 150;
            let x = (screen.width as u64 - rect_width) / 2;
            let y = (screen.height as u64 - rect_height) / 2;

            if sys_draw_rect(x, y, rect_width, rect_height, BLUE) == 0 {
                if sys_render() != 0 {
                    println!("Failed to render");
                }
            } else {
                println!("Failed to draw rectangle");
            }
        }
        Err(e) => {
            println!("Failed to get screen info: {:?}", e);
        }
    }

    0
}
