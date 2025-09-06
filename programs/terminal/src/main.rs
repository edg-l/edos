#![no_std]
#![no_main]

use elibc::{
    graphics::{sys_draw_rect, sys_render, sys_screen_info},
    println, sys_getpid,
};
use spin::Once;


const RED: u32 = 0xFF0000FF;
const GREEN: u32 = 0x00FF00FF;
const BLUE: u32 = 0x0000FFFF;

// This will be called by elibc's _start function
#[unsafe(no_mangle)]
pub extern "C" fn main() -> i32 {

    let screen = sys_screen_info().unwrap();

    0
}
