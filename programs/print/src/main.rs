#![no_std]
#![no_main]

use core::{
    ffi::{CStr, c_char},
    ptr::null_mut,
};

use elibc::{
    println,
    process::{thread_create, thread_join},
};

#[unsafe(no_mangle)]
extern "C" fn main(argc: isize, argv: *const *const u8) -> i32 {
    println!("hello world from program");
    println!("argc = {argc}");

    let argc_count = if argc < 0 { 0 } else { argc as usize };

    if !argv.is_null() {
        for index in 0..argc_count {
            let arg_ptr = unsafe { *argv.add(index) };
            if arg_ptr.is_null() {
                continue;
            }

            let text = unsafe { CStr::from_ptr(arg_ptr as *const c_char) };
            if let Ok(argument) = text.to_str() {
                println!("argv[{index}] = {argument}");
            }
        }
    }

    let id = elibc::process::sys_getpid();
    println!("hi from process {id}");

    let pid = thread_create(thread_fn, null_mut()).unwrap();

    thread_join(pid).unwrap();

    0
}

extern "C" fn thread_fn(_args: *mut u8) -> i32 {
    let id = elibc::process::sys_getpid();
    println!("hi from thread {id}");

    0
}
