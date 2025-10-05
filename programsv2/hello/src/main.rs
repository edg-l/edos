use std::{cell::Cell, fs::File, hint::black_box, io::Write, thread};

thread_local! {
    static HELLO: Cell<u64> = Cell::new(2);
}

fn main() {
    println!("{:?}", thread::current());
    println!("spawning..");
    println!("m {:?}", HELLO.get());
    HELLO.set(4);
    println!("m {:?}", HELLO.get());

    thread::spawn(|| {
        println!("hello from thread {:?}", thread::current());
        println!("t {:?}", HELLO.get());
        HELLO.set(6);
        println!("t {:?}", HELLO.get());
    })
    .join()
    .unwrap();

    println!("hello after joining");
    println!("m {:?}", HELLO.get());
    println!("{:?}", thread::current());
}
