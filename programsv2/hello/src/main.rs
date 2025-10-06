use std::{
    cell::Cell,
    thread::{self, sleep},
    time::Duration,
};

use edos_render::graphics::{Color, Screen};

thread_local! {
    static HELLO: Cell<u64> = Cell::new(2);
}

fn main() {
    println!("{:?}", thread::current());
    println!("spawning..");
    println!("m {:?}", HELLO.get());
    HELLO.set(4);
    println!("m {:?}", HELLO.get());

    let mut screen = Screen::get().unwrap();

    loop {
        screen.fill(Color::CYAN).unwrap();
        screen.render().unwrap();
        sleep(Duration::from_millis(10));
    }
}
