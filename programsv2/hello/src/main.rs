use std::{fs::File, io::Write};

fn main() {
    let mut x = Vec::new();

    let mut f = File::create("/test.txt").unwrap();

    f.write(c"hello worldo!".to_bytes()).unwrap();

    for i in 0..100 {
        x.push(i * 2);
    }
    println!("Hello, world!, {x:?}");
}
