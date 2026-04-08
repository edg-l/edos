fn main() {
    loop {
        let v: Vec<u32> = vec![0u32; 256];
        core::hint::black_box(&v); // Prevent optimization
        drop(v);
    }
}
