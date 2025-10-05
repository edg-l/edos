use std::{cell::Cell, fs::File, hint::black_box, io::Write, thread};




fn main() {

    thread::spawn(|| {
        println!("hello from thread");
    }).join();
}
