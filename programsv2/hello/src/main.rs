fn main() {
    let mut x = Vec::new();

    for i in 0..100 {
        x.push(i * 2);
    }
    println!("Hello, world!, {x:?}");
}
