use std::{cell::Cell, fs::File, hint::black_box, io::Write, thread};




fn main() {

    println!("{:?}", thread::current());
    println!("spawning..");

    thread::spawn(|| {
        println!("hello from thread {:?}", thread::current());
    }).join();

    println!("hello after joining");
     println!("{:?}", thread::current());
}
