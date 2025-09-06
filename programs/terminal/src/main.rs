#![no_std]
#![no_main]

use elibc::{
    graphics::{Color, Screen},
    println, sys_getpid, sys_read, sys_write,
};
use spin::Once;

const RED: u32 = 0xFF0000FF;
const GREEN: u32 = 0x00FF00FF;
const BLUE: u32 = 0x0000FFFF;

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {
    let screen = Screen::get().unwrap();

    screen.draw_rect(600, 20, 50, 60, Color::RED).unwrap();
    screen.render().unwrap();

    println!("hello world");
    0
}
